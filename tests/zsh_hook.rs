//! The zsh hook, installed the way `--cleanup` installs it and run in a real
//! interactive zsh (`zsh -f -i`, so none of the machine's rc files load). A
//! stub `herdr` writes down every call instead of talking to a server.
//!
//! zsh runs preexec and precmd for commands read from a pipe too, so no pty
//! is needed. Lines written all at once are what typing ahead looks like to
//! the hook. A `Pause` counts from when a line was sent, not from when zsh
//! ran it, and a slow machine can start zsh late, so a person waiting at the
//! prompt is `WaitIdle` then a `Pause`. The threshold is 1 s and the long
//! commands sleep for 1.6 s, so each test takes a few seconds.
//!
//! The stub answers `pane get` with a captured reply, a plain shell pane
//! unless a test picks another.

mod support;

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use herdr_nudge::shell_hook;
use herdr_nudge::state::{AgentsCache, StateDir};
use support::{Recorded, Replay, scratch_dir};

const PANE: &str = "w9:p1";
const SOURCE: &str = "herdr-nudge-zsh";

/// Logs each call as one line, arguments split by the unit separator so a
/// title with spaces stays one argument. `pane get` prints the reply files
/// next to the log.
const STUB: &str = r#"#!/bin/sh
IFS=$(printf '\037')
printf '%s\n' "$*" >> "$STUB_LOG"
[ "$1" = pane ] && [ "$2" = get ] || exit 0
d=$(dirname "$STUB_LOG")
[ -f "$d/pane-get.sleep" ] && sleep "$(cat "$d/pane-get.sleep")"
cat "$d/pane-get.out"
cat "$d/pane-get.err" >&2
exit "$(cat "$d/pane-get.code")"
"#;

enum Step<'a> {
    Line(&'a str),
    Pause(u64),
    /// Until an `idle` report is logged. precmd notes the prompt's time
    /// before it sends one, so a pause after this is a pause at the prompt.
    WaitIdle,
    /// Until this file exists in the shell's directory: a command that
    /// creates it has started, so its preexec has run.
    WaitFile(&'a str),
    /// SIGKILL the shell, so no precmd or zshexit runs.
    Kill,
}

use Step::{Kill, Line, Pause, WaitFile, WaitIdle};

/// Checks every 20 ms until `done` or `timeout`, and says which.
fn poll(timeout: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while !done() {
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(20));
    }
    true
}

struct Shell {
    dir: PathBuf,
    state: StateDir,
    log: PathBuf,
    /// Extra environment for zsh, or a variable to leave out.
    env: Vec<(&'static str, Option<String>)>,
    /// A line run before the hook is sourced, as `.zshrc` would.
    before: Option<&'static str>,
}

impl Shell {
    /// A state directory with the hook and `shell.env` written by the real
    /// installer, from `config` (a `config.toml`) and an agent list with
    /// `claude` and `cursor` in it.
    fn new(test: &str, config: &str) -> Shell {
        let dir = scratch_dir(test);
        let config_dir = dir.join("config");
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(config_dir.join("config.toml"), config).unwrap();

        let state = StateDir::new(dir.join("state"));
        state
            .save_agents_cache(&AgentsCache::new(
                ["claude".to_owned(), "cursor".to_owned()],
                0,
            ))
            .unwrap();
        let notes = shell_hook::install(&state, Some(&config_dir), None, None, &Replay::new([]));
        assert!(
            notes.iter().any(|n| n.starts_with("zsh hook written")),
            "install notes: {notes:?}"
        );

        let stub = dir.join("herdr");
        fs::write(&stub, STUB).unwrap();
        make_executable(&stub);
        // The first run of a new executable can take seconds while macOS
        // checks it. Get that over with here, or a watcher's call could land
        // after a test has stopped looking for it.
        let warm = Command::new(&stub)
            .env("STUB_LOG", "/dev/null")
            .status()
            .unwrap();
        assert!(warm.success());

        let shell = Shell {
            log: dir.join("herdr-calls.log"),
            dir,
            state,
            env: Vec::new(),
            before: None,
        };
        shell.answer_pane_get("pane-get-unfocused");
        shell
    }

    /// `pane get` answers as `tests/fixtures/cli/<name>.json` did.
    fn answer_pane_get(&self, name: &str) {
        let reply = Recorded::cli(name);
        fs::write(self.dir.join("pane-get.out"), &reply.stdout).unwrap();
        fs::write(self.dir.join("pane-get.err"), &reply.stderr).unwrap();
        fs::write(self.dir.join("pane-get.code"), reply.exit_code.to_string()).unwrap();
    }

    /// `pane get` takes this long to answer, as against a stopped server.
    fn delay_pane_get(&self, seconds: u32) {
        fs::write(self.dir.join("pane-get.sleep"), seconds.to_string()).unwrap();
    }

    fn env(mut self, name: &'static str, value: Option<&str>) -> Shell {
        self.env.push((name, value.map(str::to_owned)));
        self
    }

    fn before(mut self, line: &'static str) -> Shell {
        self.before = Some(line);
        self
    }

    fn source_line(&self) -> String {
        format!("source {}", self.state.shell_hook_path().display())
    }

    /// Sources the hook and writes `lines` all at once, so each is typed
    /// ahead of the one before. See [`Shell::steps`].
    fn run(&self, lines: &[&str]) -> String {
        let steps: Vec<Step> = lines.iter().map(|l| Line(l)).collect();
        self.steps(&steps)
    }

    /// Sources the hook, then feeds zsh `steps`, then closes its input, which
    /// exits the shell without an `exit` command. Returns once zsh has
    /// exited.
    fn steps(&self, steps: &[Step]) -> String {
        let stderr_path = self.dir.join("zsh-stderr.log");

        let mut cmd = Command::new("/bin/zsh");
        cmd.args(["-f", "-i"])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &self.dir)
            .env("HERDR_ENV", "1")
            .env("HERDR_PANE_ID", PANE)
            .env("HERDR_BIN_PATH", self.dir.join("herdr"))
            .env("STUB_LOG", &self.log)
            .current_dir(&self.dir)
            .stdin(Stdio::piped())
            // Files, not pipes: a background job left running would hold a
            // pipe open and the test would wait for it.
            .stdout(fs::File::create(self.dir.join("zsh-stdout.log")).unwrap())
            .stderr(fs::File::create(&stderr_path).unwrap());
        for (name, value) in &self.env {
            match value {
                Some(v) => cmd.env(name, v),
                None => cmd.env_remove(name),
            };
        }

        let mut child = cmd.spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();
        // Write errors are ignored: after a Kill, or an exec, nobody reads.
        if let Some(line) = self.before {
            let _ = writeln!(stdin, "{line}");
        }
        let _ = writeln!(stdin, "{}", self.source_line());
        for step in steps {
            match step {
                Line(line) => {
                    let _ = writeln!(stdin, "{line}");
                }
                Pause(ms) => thread::sleep(Duration::from_millis(*ms)),
                WaitIdle | WaitFile(_) => {
                    let arrived = poll(Duration::from_secs(10), || match step {
                        WaitFile(name) => self.dir.join(name).exists(),
                        _ => self
                            .read_calls()
                            .iter()
                            .any(|c| arg_after(c, "--state") == Some("idle")),
                    });
                    if !arrived {
                        // Left running, zsh and its watcher would go on
                        // writing into the scratch dir.
                        let _ = child.kill();
                        let _ = child.wait();
                        let what = match step {
                            WaitFile(name) => format!("{name} created"),
                            _ => "an idle report".to_owned(),
                        };
                        panic!("no {what} after 10 s: {:#?}", self.read_calls());
                    }
                }
                Kill => {
                    child.kill().unwrap();
                    break;
                }
            }
        }
        drop(stdin);

        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                panic!("zsh still running after 20 s");
            }
            thread::sleep(Duration::from_millis(20));
        }

        let stderr = fs::read_to_string(&stderr_path).unwrap();
        // zsh names the file or the function in any error it prints.
        assert!(
            !stderr.contains("herdr-nudge.zsh:") && !stderr.contains("_herdr_nudge_"),
            "zsh reported an error from the hook:\n{stderr}"
        );
        stderr
    }

    /// Every call so far, once `count` have arrived (or after 5 s), plus
    /// anything that turns up in the next half second.
    fn calls(&self, count: usize) -> Vec<Vec<String>> {
        poll(Duration::from_secs(5), || self.read_calls().len() >= count);
        thread::sleep(Duration::from_millis(500));
        self.read_calls()
    }

    /// No call at all, waiting long enough for a watcher that wasn't killed
    /// to have fired.
    fn assert_no_calls(&self) {
        thread::sleep(Duration::from_millis(1500));
        let calls = self.read_calls();
        assert!(calls.is_empty(), "expected no herdr calls, got {calls:#?}");
    }

    /// The watcher's note files and the shell's token file are all gone
    /// once the shell has exited.
    fn assert_no_marks(&self) {
        let marks: Vec<_> = fs::read_dir(&self.state.root)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|n| {
                let n = n.to_string_lossy();
                n.starts_with("zsh-skip") || n.starts_with("zsh-shell")
            })
            .collect();
        assert!(marks.is_empty(), "left behind: {marks:?}");
    }

    fn read_calls(&self) -> Vec<Vec<String>> {
        fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(|l| l.split('\u{1f}').map(str::to_owned).collect())
            .collect()
    }
}

fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_micros() as u64
}

/// The calls that report to Herdr, without the `pane get` queries.
fn reports(calls: &[Vec<String>]) -> Vec<Vec<String>> {
    calls.iter().filter(|c| !is_pane_get(c)).cloned().collect()
}

fn is_pane_get(call: &[String]) -> bool {
    call.len() > 2 && call[0] == "pane" && call[1] == "get" && call[2] == PANE
}

fn arg_after<'a>(call: &'a [String], flag: &str) -> Option<&'a str> {
    let i = call.iter().position(|a| a == flag)?;
    call.get(i + 1).map(String::as_str)
}

fn seq(call: &[String]) -> u64 {
    arg_after(call, "--seq")
        .unwrap_or_else(|| panic!("no --seq in {call:?}"))
        .parse()
        .unwrap()
}

/// The one call whose first three words are `pane <verb> <pane>` and that
/// has `extra` (a flag and its value) in it.
fn find<'a>(calls: &'a [Vec<String>], verb: &str, extra: (&str, &str)) -> &'a [String] {
    let found: Vec<_> = calls
        .iter()
        .filter(|c| c.len() > 2 && c[0] == "pane" && c[1] == verb && c[2] == PANE)
        .filter(|c| arg_after(c, extra.0) == Some(extra.1))
        .collect();
    assert_eq!(
        found.len(),
        1,
        "pane {verb} with {} {}: {calls:#?}",
        extra.0,
        extra.1
    );
    found[0]
}

const CONFIG: &str = "[shell]\nmin_seconds = 1\nignore_commands = [\"vim\"]\n";

#[test]
fn a_command_under_the_threshold_never_calls_herdr() {
    let shell = Shell::new("zsh_short", CONFIG);
    shell.run(&["sleep 0.3", "true", "false"]);
    shell.assert_no_calls();
}

#[test]
fn a_long_command_reports_working_then_its_result_then_releases() {
    let shell = Shell::new("zsh_long", CONFIG);
    let before = now_us();
    shell.run(&["sleep 1.6", "true"]);
    let calls = shell.calls(7);
    assert_eq!(calls.len(), 7, "{calls:#?}");
    assert_eq!(
        calls.iter().filter(|c| is_pane_get(c)).count(),
        1,
        "{calls:#?}"
    );
    let calls = reports(&calls);
    for call in &calls {
        assert_eq!(arg_after(call, "--source"), Some(SOURCE), "{call:?}");
        assert!(seq(call) > before, "--seq below the clock: {call:?}");
    }

    // The watcher replaces the last command's title before it claims the
    // pane.
    let clear = find(&calls, "report-metadata", ("--title", "sleep 1.6"));
    assert!(
        clear.contains(&"--clear-state-labels".to_owned()),
        "{clear:?}"
    );
    let working = find(&calls, "report-agent", ("--state", "working"));
    assert_eq!(arg_after(working, "--agent"), Some("sleep"));

    let result = find(&calls, "report-metadata", ("--state-label", "idle=done"));
    assert_eq!(
        arg_after(result, "--title"),
        Some("sleep 1.6 · exit 0 · 1s")
    );
    let idle = find(&calls, "report-agent", ("--state", "idle"));
    assert_eq!(arg_after(idle, "--agent"), Some("sleep"));

    // `true` was typed ahead, so it leaves the claim alone; the shell
    // exiting releases it.
    let release = find(&calls, "release-agent", ("--agent", "sleep"));

    // Then the title and labels come off. On a pane nobody claims, that is
    // the event that tells the plugin the user moved on, so it has to reach
    // Herdr after the release.
    let cleared = cleared(&calls);
    assert_eq!(cleared.len(), 1, "{calls:#?}");
    let (cleared_at, cleared) = cleared[0];
    let released_at = calls.iter().position(|c| c.as_slice() == release).unwrap();
    assert!(
        released_at < cleared_at,
        "cleared before the release: {calls:#?}"
    );

    // Herdr keeps one --seq for reports and releases, and another for
    // metadata, and drops anything that doesn't go up.
    assert!(seq(working) < seq(idle) && seq(idle) < seq(release));
    assert!(seq(clear) < seq(result) && seq(result) < seq(cleared));
}

/// The metadata reports that take the title and labels off, with where
/// each is in `calls`.
fn cleared(calls: &[Vec<String>]) -> Vec<(usize, &Vec<String>)> {
    calls
        .iter()
        .enumerate()
        .filter(|(_, c)| c[1] == "report-metadata" && c.contains(&"--clear-title".to_owned()))
        .inspect(|(_, c)| assert!(c.contains(&"--clear-state-labels".to_owned()), "{c:?}"))
        .collect()
}

fn releases(calls: &[Vec<String>]) -> Vec<&Vec<String>> {
    calls.iter().filter(|c| c[1] == "release-agent").collect()
}

#[test]
fn a_command_typed_at_the_prompt_releases_the_claim() {
    // `true` starts 1.5 s after the prompt, well past the hook's 1 s
    // typed-ahead window. A fixed pause from sending the first line failed
    // on a slow CI runner, where zsh started that command late. The sleeps
    // are longer than elsewhere so the watcher has 1.5 s after its 1 s
    // threshold to ask and claim before precmd stops it.
    let shell = Shell::new("zsh_release_at_prompt", CONFIG);
    shell.steps(&[
        Line("sleep 2.5"),
        WaitIdle,
        Pause(1500),
        Line("true"),
        Line("sleep 2.5"),
    ]);
    let calls = shell.calls(14);
    let released = releases(&calls);
    assert_eq!(
        released.len(),
        2,
        "one from `true`, one on exit: {calls:#?}"
    );
    assert_eq!(
        cleared(&calls).len(),
        2,
        "one after each release: {calls:#?}"
    );
    let first_idle = calls
        .iter()
        .filter(|c| arg_after(c, "--state") == Some("idle"))
        .map(|c| seq(c))
        .min()
        .unwrap();
    let first_release = released.iter().map(|c| seq(c)).min().unwrap();
    assert!(first_idle < first_release, "release before the first idle");
}

#[test]
fn a_command_typed_ahead_keeps_the_claim() {
    // `true` starts milliseconds after `sleep 1.6` ends. Releasing then would
    // take down, or race, the banner for a command the user walked away from.
    let shell = Shell::new("zsh_typed_ahead", CONFIG);
    shell.run(&["sleep 1.6", "true", "sleep 1.6"]);
    let calls = shell.calls(12);
    let released = releases(&calls);
    assert_eq!(
        released.len(),
        1,
        "only the shell exiting releases: {calls:#?}"
    );
    // A clear on `true` would take down the banner for `sleep 1.6` on
    // Herdr 0.9.2, which releases the pane by itself.
    assert_eq!(
        cleared(&calls).len(),
        1,
        "only the shell exiting clears: {calls:#?}"
    );
    let last_idle = calls
        .iter()
        .filter(|c| arg_after(c, "--state") == Some("idle"))
        .map(|c| seq(c))
        .max()
        .unwrap();
    assert!(seq(released[0]) > last_idle);
}

#[test]
fn a_slow_prompt_does_not_make_a_typed_ahead_command_release() {
    // A precmd hook after ours, like a slow git prompt. Only whether the
    // next line was already waiting counts, not how long the prompt took.
    let shell = Shell::new("zsh_slow_prompt", CONFIG);
    shell.run(&[
        "slow() { sleep 1.2 }; precmd_functions+=(slow)",
        "sleep 1.6",
        "true",
        "sleep 1.6",
    ]);
    let calls = shell.calls(12);
    let released = releases(&calls);
    assert_eq!(
        released.len(),
        1,
        "only the shell exiting releases: {calls:#?}"
    );
    assert_eq!(
        cleared(&calls).len(),
        1,
        "only the shell exiting clears: {calls:#?}"
    );
}

#[test]
fn the_hook_leaves_the_users_reply_and_last_job_alone() {
    // A long command, then one typed at the prompt, which releases: every
    // path that starts something in the background.
    let shell = Shell::new("zsh_user_parameters", CONFIG);
    shell.steps(&[
        Line("REPLY=mine; sleep 30 & job=$!"),
        Line("sleep 1.6"),
        WaitIdle,
        Pause(1500),
        Line("true"),
        // Unquoted: `!"` would be history expansion in an interactive zsh.
        Line("print -r -- $REPLY $job $! > kept; kill $job"),
    ]);
    let kept = fs::read_to_string(shell.dir.join("kept")).unwrap();
    let words: Vec<_> = kept.split_whitespace().collect();
    assert_eq!(words.len(), 3, "kept: {kept:?}");
    assert_eq!(words[0], "mine", "REPLY after the hook ran: {kept:?}");
    assert_eq!(words[1], words[2], "$! after the hook ran: {kept:?}");
    let calls = shell.calls(7);
    assert_eq!(releases(&calls).len(), 1, "`true` releases: {calls:#?}");
}

#[test]
fn a_failed_long_command_is_labelled_failed() {
    let shell = Shell::new("zsh_failed", CONFIG);
    shell.run(&["sleep 1.6; false"]);
    let calls = shell.calls(7);
    let result = find(&calls, "report-metadata", ("--state-label", "idle=failed"));
    assert_eq!(
        arg_after(result, "--title"),
        Some("sleep 1.6; false · exit 1 · 1s")
    );
}

#[test]
fn the_shell_exiting_releases_the_claim() {
    // No command after the long one: end of input exits the shell with no
    // preexec, so only zshexit can release.
    let shell = Shell::new("zsh_exit", CONFIG);
    shell.run(&["sleep 1.6"]);
    let calls = shell.calls(7);
    let idle = find(&calls, "report-agent", ("--state", "idle"));
    let release = find(&calls, "release-agent", ("--agent", "sleep"));
    assert!(seq(idle) < seq(release));
    assert_eq!(cleared(&calls).len(), 1, "{calls:#?}");
}

#[test]
fn ignored_commands_and_agent_clis_are_not_reported() {
    // Functions stand in for the real programs. `nap` is an alias, so only
    // its expansion is on the lists. Quotes and an assignment with a path in
    // it must not hide the name.
    let shell = Shell::new("zsh_skipped", CONFIG);
    shell.run(&[
        "vim() { sleep 1.3 }",
        "claude() { sleep 1.3 }",
        "alias nap=vim",
        "vim",
        "claude",
        "nap",
        "\\vim",
        "'vim' a",
        "EDITOR=/opt/x vim",
        "PATH=/opt/bin:$PATH claude",
        "EDITOR=vim",
        "$EDITOR",
        "\"${EDITOR}\" a",
        "${NOPE:-claude}",
        "$NOPE vim",
    ]);
    shell.assert_no_calls();
}

/// `_herdr_nudge_program` on its own, in a shell with the hook loaded. The
/// lines go through a file so zsh never parses them as commands.
#[test]
fn the_program_a_line_runs_is_found_as_zsh_would_run_it() {
    let cases = [
        ("a=1; vim notes", "vim"),
        ("a=1 && vim notes", "vim"),
        ("$E x", "code"),
        ("\"$E\" x", "code"),
        ("'$E' x", "$E"),
        ("\\$E x", "$E"),
        ("${E:-vim} x", "code"),
        ("${E2:-vim} x", "vim"),
        ("${E2-vim} x", "x"),
        ("${UNSET-vim} x", "vim"),
        ("$UNSET vim", "vim"),
        ("=sleep 1", "sleep"),
    ];
    let shell = Shell::new("zsh_program", CONFIG);
    let lines: String = cases.iter().map(|(line, _)| format!("{line}\n")).collect();
    fs::write(shell.dir.join("lines"), lines).unwrap();
    shell.run(&[
        "E=code; E2=; unset UNSET",
        "while IFS= read -r l; do _herdr_nudge_program $l && print -r -- $REPLY || print -; done < lines > labels",
    ]);
    let labels = fs::read_to_string(shell.dir.join("labels")).unwrap();
    let labels: Vec<_> = labels.lines().collect();
    assert_eq!(labels.len(), cases.len(), "labels: {labels:?}");
    for ((line, want), got) in cases.iter().zip(labels) {
        assert_eq!(got, *want, "program of {line:?}");
    }
}

#[test]
fn a_command_named_by_a_variable_is_labelled_by_what_it_runs() {
    let shell = Shell::new("zsh_variable_command", CONFIG);
    shell.run(&["nap=sleep", "$nap 1.6", "=sleep 1.6"]);
    let calls = reports(&shell.calls(12));
    let working: Vec<_> = calls
        .iter()
        .filter(|c| arg_after(c, "--state") == Some("working"))
        .map(|c| arg_after(c, "--agent"))
        .collect();
    assert_eq!(working, [Some("sleep"), Some("sleep")], "{calls:#?}");
}

#[test]
fn exec_is_not_timed() {
    // The shell is replaced, so nothing would ever end the claim. Neither
    // form starts with `exec` as typed.
    for (name, lines) in [
        ("zsh_exec_chain", &["cd . && exec sleep 1.3"][..]),
        (
            "zsh_exec_alias",
            &["alias again='exec sleep 1.3'", "again"][..],
        ),
    ] {
        let shell = Shell::new(name, CONFIG);
        shell.run(lines);
        shell.assert_no_calls();
    }
}

#[test]
fn an_exec_hidden_in_a_function_leaves_no_claim() {
    // `omz reload` execs zsh from inside a function. The new shell has the
    // same pid, so only its new token tells the watcher. The pause lets the
    // new shell stay up until the watcher has looked.
    let slow = "reload() { sleep 1.3; exec zsh -f -i }";
    let quick = "reload() { exec zsh -f -i }";
    for (name, define) in [
        ("zsh_exec_after_claim", slow),
        ("zsh_exec_before_claim", quick),
    ] {
        let shell = Shell::new(name, CONFIG);
        let again = shell.source_line();
        shell.steps(&[Line(define), Line("reload"), Line(&again), Pause(4000)]);
        let calls = reports(&shell.calls(0));
        if define == slow {
            let working = find(&calls, "report-agent", ("--state", "working"));
            let release = find(&calls, "release-agent", ("--agent", "reload"));
            assert!(seq(working) < seq(release), "{calls:#?}");
            assert_eq!(cleared(&calls).len(), 1, "{name}: {calls:#?}");
            assert!(
                !calls
                    .iter()
                    .any(|c| arg_after(c, "--state") == Some("idle")),
                "{name}: nothing finished the command: {calls:#?}"
            );
        } else {
            assert!(calls.is_empty(), "{name}: {calls:#?}");
        }
        shell.assert_no_marks();
    }
}

#[test]
fn an_exec_typed_ahead_releases_the_claim_the_shell_still_held() {
    // `sleep 1.6` finishes with a claim, and `reload` was typed ahead, so its
    // preexec keeps that claim. Once the shell is replaced, nothing but the
    // `reload` watcher knows the claim is there.
    let shell = Shell::new("zsh_exec_after_typed_ahead", CONFIG);
    let again = shell.source_line();
    shell.steps(&[
        Line("reload() { exec zsh -f -i }"),
        Line("sleep 1.6"),
        Line("reload"),
        Line(&again),
        Pause(5000),
    ]);
    let calls = reports(&shell.calls(0));
    let idle = find(&calls, "report-agent", ("--state", "idle"));
    let release = find(&calls, "release-agent", ("--agent", "sleep"));
    assert!(seq(idle) < seq(release), "{calls:#?}");
    assert_eq!(releases(&calls).len(), 1, "{calls:#?}");
    assert_eq!(cleared(&calls).len(), 1, "{calls:#?}");
    shell.assert_no_marks();
}

#[test]
fn a_killed_shell_leaves_no_claim() {
    // SIGKILL runs no zshexit. The watcher outlives the shell and must not
    // report once its deadline comes. Killing only after `started` exists
    // makes sure there is a watcher: a fixed pause could kill a slow shell
    // before preexec, and the test would pass without one.
    let shell = Shell::new("zsh_killed", CONFIG);
    shell.steps(&[Line("touch started; sleep 3"), WaitFile("started"), Kill]);
    shell.assert_no_calls();
}

#[test]
fn sourcing_again_after_shell_commands_are_turned_off_removes_the_hooks() {
    // What `source ~/.zshrc` in an open pane does after `[shell] enabled =
    // false` and a Herdr restart.
    let shell = Shell::new("zsh_resourced_off", CONFIG);
    let env = shell.state.shell_env_path();
    let hook = shell.state.shell_hook_path();
    let off = format!("print -r -- enabled=0 > {}", env.display());
    let again = format!("source {}", hook.display());
    shell.run(&[&off, &again, "sleep 1.3"]);
    shell.assert_no_calls();
}

#[test]
fn the_hook_does_nothing_outside_a_herdr_pane() {
    let shell = Shell::new("zsh_outside", CONFIG).env("HERDR_ENV", None);
    shell.run(&["sleep 1.3"]);
    shell.assert_no_calls();
}

#[test]
fn the_hook_works_under_nounset() {
    let outside = Shell::new("zsh_nounset_outside", CONFIG)
        .env("HERDR_ENV", None)
        .before("setopt nounset");
    outside.run(&["sleep 1.3"]);
    outside.assert_no_calls();

    let inside = Shell::new("zsh_nounset_inside", CONFIG).before("setopt nounset");
    inside.run(&["sleep 1.6"]);
    let calls = inside.calls(6);
    find(&calls, "report-agent", ("--state", "working"));
    find(&calls, "report-agent", ("--state", "idle"));
}

#[test]
fn the_hook_does_nothing_when_shell_commands_are_disabled() {
    let shell = Shell::new(
        "zsh_disabled",
        "[shell]\nenabled = false\nmin_seconds = 1\n",
    );
    shell.run(&["sleep 1.3"]);
    shell.assert_no_calls();
}

#[test]
fn the_hook_does_nothing_without_shell_env() {
    let shell = Shell::new("zsh_no_env", CONFIG);
    fs::remove_file(shell.state.shell_env_path()).unwrap();
    shell.run(&["sleep 1.3"]);
    shell.assert_no_calls();
}

#[test]
fn the_hook_stands_aside_for_herdr_ohmyzsh() {
    let shell = Shell::new("zsh_omz", CONFIG);
    shell.run(&["_herdr_omz_preexec() { }", "sleep 1.3"]);
    shell.assert_no_calls();
}

#[test]
fn a_pane_herdr_detected_as_an_agent_is_left_alone() {
    // Typed as `cursor-agent`, which isn't on the agent list; Herdr knows it
    // as `cursor`, which is. `sleep` stands in for the program: only the
    // reply decides.
    let shell = Shell::new("zsh_detected_alias", CONFIG);
    shell.answer_pane_get("pane-get-detected-alias");
    shell.run(&["sleep 1.6", "true"]);
    let calls = shell.calls(1);
    assert!(
        calls.len() == 1 && is_pane_get(&calls[0]),
        "expected only the pane get, got {calls:#?}"
    );
    shell.assert_no_marks();
}

#[test]
fn a_pane_an_agent_reports_on_itself_is_left_alone() {
    // Claude through its own integration: `claude`, with a session.
    let shell = Shell::new("zsh_agent_reports", CONFIG);
    shell.answer_pane_get("pane-get-focused");
    shell.run(&["sleep 1.6"]);
    let calls = shell.calls(1);
    assert!(
        calls.len() == 1 && is_pane_get(&calls[0]),
        "expected only the pane get, got {calls:#?}"
    );
}

#[test]
fn a_pane_another_shell_command_holds_is_claimed() {
    // `make`, from another shell or a shell that was killed, isn't an agent.
    let shell = Shell::new("zsh_other_shell", CONFIG);
    shell.answer_pane_get("pane-get-unfocused-0.9.1");
    shell.run(&["sleep 1.6"]);
    let calls = shell.calls(6);
    find(&calls, "report-agent", ("--state", "working"));
    find(&calls, "report-agent", ("--state", "idle"));
}

#[test]
fn a_failed_pane_get_still_claims() {
    let shell = Shell::new("zsh_pane_get_fails", CONFIG);
    shell.answer_pane_get("pane-get-not-found");
    shell.run(&["sleep 1.6"]);
    let calls = shell.calls(6);
    find(&calls, "report-agent", ("--state", "working"));
    find(&calls, "report-agent", ("--state", "idle"));
}

#[test]
fn a_pane_get_that_never_answers_still_claims_after_two_seconds() {
    // The reply would say agent, but it comes too late to count.
    let shell = Shell::new("zsh_pane_get_hangs", CONFIG);
    shell.answer_pane_get("pane-get-detected-alias");
    shell.delay_pane_get(10);
    // The claim comes at about 3 s: the 1 s threshold, then 2 s waiting.
    let started = Instant::now();
    shell.run(&["sleep 4.5"]);
    let calls = shell.calls(6);
    find(&calls, "report-agent", ("--state", "working"));
    find(&calls, "report-agent", ("--state", "idle"));
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "zsh waited on pane get"
    );
}

#[test]
fn a_command_that_ends_while_the_hook_is_still_asking_is_not_reported() {
    // Nothing was claimed, so there's no claim to finish.
    let shell = Shell::new("zsh_ends_while_asking", CONFIG);
    shell.delay_pane_get(10);
    shell.run(&["sleep 1.8"]);
    shell.assert_no_marks();
    let calls = shell.calls(1);
    assert!(
        calls.len() == 1 && is_pane_get(&calls[0]),
        "expected only the pane get, got {calls:#?}"
    );
}
