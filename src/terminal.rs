//! Which macOS app to bring forward with `open -b` before a click focuses the
//! pane, and whether the user is already looking at it.
//!
//! Herdr never tells a plugin which terminal it is shown in. Each attached
//! `herdr` client process does carry it, though: `__CFBundleIdentifier` in its
//! environment is the app it was started from (Ghostty, iTerm, Terminal). So
//! we find the clients attached to our server and read their environments.
//!
//! The server's own environment is no help. The server is persistent, so it
//! keeps naming the terminal the first client was started from after the user
//! has moved on to another one. And a server started from Raycast or an IDE
//! names that app, which `open -b` would then relaunch on every click.
//!
//! The parent-process chain is no help either: iTerm runs its shells under an
//! `iTermServer` daemon, not under the app.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::config::Config;
use crate::process::Runner;

const LSAPPINFO: &str = "/usr/bin/lsappinfo";
const PGREP: &str = "/usr/bin/pgrep";
const PS: &str = "/bin/ps";
const LSOF: &str = "/usr/sbin/lsof";

/// Every process whose command line starts with `herdr`, with or without a
/// path, or with the `-` a login shell gets: iTerm2 runs a custom shell as
/// `-herdr`. `herdr-nudge` (us, and any other hook running right now)
/// doesn't match. A `herdr` installed under a path with a space in it
/// doesn't either.
pub const HERDR_PATTERN: &str = "^-?([^ ]*/)?herdr( |$)";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalSource {
    DefaultConfig,
    /// The terminal of an attached Herdr client.
    Client,
    Unresolved,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    /// `None` means the click focuses the pane without bringing any app
    /// forward. The notification is still sent.
    pub bundle_id: Option<String>,
    pub source: TerminalSource,
    /// Every terminal Herdr is on screen in. With two clients in two
    /// terminals, either one in front means the user can see the focused
    /// pane, even though only one of them is what a click raises.
    pub showing: BTreeSet<String>,
}

impl Resolution {
    /// Whether one of the terminals Herdr is shown in is the frontmost app.
    /// Asks `lsappinfo` only when there is a terminal to compare with.
    pub fn in_front(&self, runner: &impl Runner) -> bool {
        !self.showing.is_empty()
            && frontmost_bundle_id(runner).is_some_and(|front| self.showing.contains(&front))
    }
}

/// `default_terminal` if set, otherwise the terminal of whichever attached
/// client the user was in last.
///
/// The config goes first because the only reason to set it is a detection
/// that comes out wrong, as it does for a client run inside tmux, which
/// reports the terminal tmux was started from.
///
/// `socket_path` is the server's API socket, `HERDR_SOCKET_PATH`. It is what
/// tells our server apart from any other session's.
pub fn resolve(
    config: &Config,
    runner: &impl Runner,
    socket_path: &Path,
    notes: &mut Vec<String>,
) -> Resolution {
    if let Some(bundle) = &config.default_terminal {
        return Resolution {
            bundle_id: Some(bundle.clone()),
            source: TerminalSource::DefaultConfig,
            showing: BTreeSet::from([bundle.clone()]),
        };
    }
    let (clients, picked) = detect(runner, socket_path, notes);
    match picked {
        Some(bundle) => Resolution {
            bundle_id: Some(bundle),
            source: TerminalSource::Client,
            showing: terminals(&clients),
        },
        None => Resolution {
            bundle_id: None,
            source: TerminalSource::Unresolved,
            showing: BTreeSet::new(),
        },
    }
}

/// The clients attached to the server behind `socket_path`, and the terminal
/// of the one used last. Logs one line saying what it found, for `herdr
/// plugin log`: `clients 21836=com.googlecode.iterm2 86128=- -> …`.
pub fn detect(
    runner: &impl Runner,
    socket_path: &Path,
    notes: &mut Vec<String>,
) -> (Vec<Client>, Option<String>) {
    let clients = attached_clients(runner, socket_path, notes);
    let picked = pick(runner, &clients, notes);
    let mut line = String::from("clients");
    for c in &clients {
        let bundle = c.bundle_id.as_deref().unwrap_or("-");
        line.push_str(&format!(" {}={bundle}", c.pid));
    }
    if clients.is_empty() {
        line.push_str(" none");
    }
    line.push_str(&format!(" -> {}", picked.as_deref().unwrap_or("-")));
    notes.push(line);
    (clients, picked)
}

/// A `herdr` process attached to our server as a client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Client {
    pub pid: u32,
    /// Seconds since it started. Only used when none of the clients'
    /// terminals is on screen, to take the newest.
    pub age_secs: u64,
    /// `None` for a client started outside any app, e.g. over ssh.
    pub bundle_id: Option<String>,
}

/// The clients attached to the server behind `socket_path`, with their
/// terminals.
///
/// Which server a client belongs to can't be read off its command line: a
/// session can be named with `--session x`, `--session=x`, `session attach
/// x`, or `HERDR_SESSION` in its environment. So it is read off the sockets
/// instead. Every client holds a connection to one of its server's named
/// sockets, and the server is the process holding `socket_path`.
///
/// Anything that goes wrong is noted and gives fewer clients, never an error:
/// the notification is worth sending without a terminal to raise.
pub fn attached_clients(
    runner: &impl Runner,
    socket_path: &Path,
    notes: &mut Vec<String>,
) -> Vec<Client> {
    let listed = herdr_processes(runner, notes);
    let candidates: Vec<u32> = listed
        .iter()
        .filter(|(_, args)| is_client(args))
        .map(|(pid, _)| *pid)
        .collect();
    if candidates.is_empty() {
        return Vec::new();
    }
    let servers = listed
        .iter()
        .filter(|(_, args)| is_server(args))
        .map(|(pid, _)| *pid);
    let ours = attached_to(runner, socket_path, servers, &candidates, notes);
    if ours.is_empty() {
        return Vec::new();
    }

    let list = join_pids(&ours);
    let out = match runner.run(
        Path::new(PS),
        &["-Eww", "-o", "pid=,etime=,command=", "-p", &list],
    ) {
        Ok(out) if out.success() => out.stdout,
        Ok(out) => {
            notes.push(format!("ps exited {:?}: {}", out.code, out.stderr.trim()));
            return Vec::new();
        }
        Err(e) => {
            notes.push(format!("ps: {e}"));
            return Vec::new();
        }
    };
    // ps prints the arguments before the environment, the same words pgrep
    // printed, so they can be skipped.
    parse_ps(&out, |pid| {
        listed
            .iter()
            .find(|(p, _)| *p == pid)
            .map(|(_, args)| args.len())
    })
    .into_iter()
    .filter(|c| ours.contains(&c.pid))
    .collect()
}

/// Which of `candidates` hold a connection to a named socket of the server
/// that holds `socket_path`, going by the unix-socket addresses `lsof` shows.
///
/// If `lsof` can't say, every candidate is kept and a note says so. A client
/// of another session then counts as ours: its terminal can be raised, and
/// with it in front a focused pane counts as watched.
fn attached_to(
    runner: &impl Runner,
    socket_path: &Path,
    servers: impl Iterator<Item = u32>,
    candidates: &[u32],
    notes: &mut Vec<String>,
) -> Vec<u32> {
    let mut all: Vec<u32> = servers.chain(candidates.iter().copied()).collect();
    all.sort_unstable();
    let list = join_pids(&all);
    // `-b` stops lsof calling stat, lstat and readlink. The click has to
    // stay out of ~/Documents and the like, and a client started from a
    // folder in there has its working directory there.
    //
    // A pid that has exited since pgrep makes lsof exit 1 but still print
    // the rest, so the output is read whatever the exit code.
    let out = match runner.run(
        Path::new(LSOF),
        &["-b", "-w", "-U", "-a", "-p", &list, "-F", "dn"],
    ) {
        Ok(out) if !out.stdout.is_empty() => out.stdout,
        Ok(out) => {
            notes.push(format!("lsof exited {:?}, keeping every client", out.code));
            return candidates.to_vec();
        }
        Err(e) => {
            notes.push(format!("lsof: {e}, keeping every client"));
            return candidates.to_vec();
        }
    };
    let sockets = parse_lsof(&out);
    let socket_path = socket_path.to_string_lossy();
    let Some(server) = sockets
        .iter()
        .find(|s| s.name == socket_path)
        .map(|s| s.pid)
    else {
        notes.push(format!(
            "no process holds {socket_path}, keeping every client"
        ));
        return candidates.to_vec();
    };
    let named: BTreeSet<&str> = sockets
        .iter()
        .filter(|s| s.pid == server && !s.name.starts_with("->"))
        .map(|s| s.device.as_str())
        .collect();
    candidates
        .iter()
        .copied()
        .filter(|pid| {
            sockets.iter().any(|s| {
                s.pid == *pid
                    && s.name
                        .strip_prefix("->")
                        .is_some_and(|peer| named.contains(peer))
            })
        })
        .collect()
}

/// The terminal a click should raise: the one of those showing Herdr that the
/// user was in last.
///
/// `lsappinfo visibleProcessList` lists apps most recently used first. A
/// hidden app isn't in it at all, so when every client's terminal is hidden
/// the newest client's terminal is taken.
pub fn pick(runner: &impl Runner, clients: &[Client], notes: &mut Vec<String>) -> Option<String> {
    let terminals = terminals(clients);
    if terminals.len() < 2 {
        return terminals.into_iter().next();
    }

    let visible = match runner.run(Path::new(LSAPPINFO), &["visibleProcessList"]) {
        Ok(out) if out.success() => asns(&out.stdout),
        Ok(out) => {
            notes.push(format!(
                "lsappinfo visibleProcessList exited {:?}",
                out.code
            ));
            Vec::new()
        }
        Err(e) => {
            notes.push(format!("lsappinfo visibleProcessList: {e}"));
            Vec::new()
        }
    };
    let ranked = terminals
        .iter()
        .filter_map(|bundle| {
            let arg = format!("bundleid={bundle}");
            let found = runner.run(Path::new(LSAPPINFO), &["find", &arg]).ok()?;
            let rank = asns(&found.stdout)
                .iter()
                .filter_map(|asn| visible.iter().position(|v| v == asn))
                .min()?;
            Some((rank, bundle))
        })
        .min();
    if let Some((_, bundle)) = ranked {
        return Some(bundle.clone());
    }

    clients
        .iter()
        .filter(|c| c.bundle_id.is_some())
        .min_by_key(|c| (c.age_secs, std::cmp::Reverse(c.pid)))
        .and_then(|c| c.bundle_id.clone())
}

fn terminals(clients: &[Client]) -> BTreeSet<String> {
    clients.iter().filter_map(|c| c.bundle_id.clone()).collect()
}

fn join_pids(pids: &[u32]) -> String {
    pids.iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

/// Every running `herdr` process with its command line, servers and clients
/// of every session. Empty, with a note, if `pgrep` fails.
pub fn herdr_processes(runner: &impl Runner, notes: &mut Vec<String>) -> Vec<(u32, Vec<String>)> {
    // `-a`: a hook runs under the server, which runs under the client that
    // started it, and pgrep leaves out its own ancestors unless told not to.
    match runner.run(Path::new(PGREP), &["-a", "-lf", HERDR_PATTERN]) {
        // 1 is "nothing matched".
        Ok(out) if out.success() || out.code == Some(1) => parse_pgrep(&out.stdout),
        Ok(out) => {
            notes.push(format!(
                "pgrep exited {:?}: {}",
                out.code,
                out.stderr.trim()
            ));
            Vec::new()
        }
        Err(e) => {
            notes.push(format!("pgrep: {e}"));
            Vec::new()
        }
    }
}

/// `herdr server`, whatever session it serves: a named session's server
/// has the name only in its environment. `herdr server stop` and the other
/// subcommands are short-lived CLI calls, not servers; our own startup hook
/// runs `herdr server agent-manifests`.
pub fn is_server(args: &[String]) -> bool {
    args.len() == 2 && args[1] == "server"
}

/// One process's environment, from `ps -Eww -o command=`, which prints the
/// command line and then the environment, space-separated. `arg_words` is
/// how many words the command line has, as `pgrep` printed it; the variables
/// are read only after those. A value with a space in it comes back cut at
/// the space, and the rest of it can look like another variable, so the
/// first of each name wins: the real one comes before anything a later
/// value could contain. `None` if `ps` fails, most often because the
/// process has gone.
pub fn process_env(
    runner: &impl Runner,
    pid: u32,
    arg_words: usize,
) -> Option<BTreeMap<String, String>> {
    let out = runner
        .run(
            Path::new(PS),
            &["-Eww", "-o", "command=", "-p", &pid.to_string()],
        )
        .ok()
        .filter(|out| out.success())?;
    let mut env = BTreeMap::new();
    for (k, v) in out
        .stdout
        .split_whitespace()
        .skip(arg_words)
        .filter_map(|w| w.split_once('='))
    {
        env.entry(k.to_owned()).or_insert_with(|| v.to_owned());
    }
    Some(env)
}

/// `pgrep -lf` lines: a pid, a space, the command line.
pub fn parse_pgrep(stdout: &str) -> Vec<(u32, Vec<String>)> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let pid = words.next()?.parse().ok()?;
            Some((pid, words.map(str::to_owned).collect()))
        })
        .collect()
}

/// Whether a `herdr` command line is a client rather than the server or a
/// CLI command. `args[0]` is the program.
///
/// A client is `herdr` alone, `herdr` with options (`--session <name>`), or
/// `herdr session attach <name>`. `--remote` attaches to a server on another
/// machine, so that client shows none of our panes. The options that print
/// something and exit aren't clients either.
pub fn is_client(args: &[String]) -> bool {
    let exits_at_once = [
        "--help",
        "-h",
        "--version",
        "-V",
        "--default-config",
        "--skill",
    ];
    let rest = args.get(1..).unwrap_or_default();
    if rest.iter().any(|a| {
        a == "--remote" || a.starts_with("--remote=") || exits_at_once.contains(&a.as_str())
    }) {
        return false;
    }
    match rest.first().map(String::as_str) {
        None => true,
        Some(first) if first.starts_with('-') => true,
        Some("session") => rest.get(1).is_some_and(|a| a == "attach"),
        Some(_) => false,
    }
}

/// `ps -E -o pid=,etime=,command=` lines: pid, elapsed time, then the command
/// line with the environment after it, all space-separated.
///
/// The two can't be told apart in that output, so `arg_words` says how many
/// words of each pid's line are its command line, and `__CFBundleIdentifier`
/// is looked for only after them. A client with no such variable gets `None`.
pub fn parse_ps(stdout: &str, arg_words: impl Fn(u32) -> Option<usize>) -> Vec<Client> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let pid = words.next()?.parse().ok()?;
            let age_secs = parse_etime(words.next()?)?;
            let bundle_id = words
                .skip(arg_words(pid).unwrap_or(0))
                .find_map(|w| w.strip_prefix("__CFBundleIdentifier="))
                .filter(|b| is_bundle_id(b))
                .map(str::to_owned);
            Some(Client {
                pid,
                age_secs,
                bundle_id,
            })
        })
        .collect()
}

/// `[[dd-]hh:]mm:ss`.
fn parse_etime(etime: &str) -> Option<u64> {
    let (days, clock) = match etime.split_once('-') {
        Some((d, rest)) => (d.parse::<u64>().ok()?, rest),
        None => (0, etime),
    };
    let mut secs = 0;
    for part in clock.split(':') {
        secs = secs * 60 + part.parse::<u64>().ok()?;
    }
    Some(days * 86_400 + secs)
}

/// The id ends up as an argument to `open -b` and in a job file, never in a
/// shell, but an odd value in someone's environment is still better dropped.
fn is_bundle_id(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
}

/// One unix socket from `lsof -F dn`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Socket {
    pub pid: u32,
    /// The socket's own address, `0x…`.
    pub device: String,
    /// A path for a named socket, `->0x…` (the peer's address) for a
    /// connection.
    pub name: String,
}

/// `lsof -F` prints one field per line, each tagged by its first letter: `p`
/// starts a process, `f` a file, and `d` and `n` belong to the file before.
pub fn parse_lsof(stdout: &str) -> Vec<Socket> {
    let mut sockets = Vec::new();
    let mut pid = None;
    let mut device = None;
    for line in stdout.lines() {
        let (tag, value) = line.split_at_checked(1).unwrap_or(("", ""));
        match tag {
            "p" => {
                pid = value.parse().ok();
                device = None;
            }
            "f" => device = None,
            "d" => device = Some(value.to_owned()),
            "n" => {
                if let (Some(pid), Some(device)) = (pid, device.take()) {
                    sockets.push(Socket {
                        pid,
                        device,
                        name: value.to_owned(),
                    });
                }
            }
            _ => {}
        }
    }
    sockets
}

/// Every `ASN:0x0-0x64b64b` in `lsappinfo` output, as `0x0-0x64b64b`, in the
/// order printed. `visibleProcessList` and `find` follow each one with
/// `-"AppName":`, `front` with a bare `:`.
pub fn asns(stdout: &str) -> Vec<String> {
    stdout
        .split("ASN:")
        .skip(1)
        .filter_map(|rest| {
            let end = rest
                .find(|c: char| !(c.is_ascii_hexdigit() || c == 'x' || c == '-'))
                .unwrap_or(rest.len());
            let asn = rest[..end].trim_end_matches('-');
            (!asn.is_empty()).then(|| asn.to_owned())
        })
        .collect()
}

/// The frontmost app's bundle id, from `lsappinfo`. It needs no permission,
/// where asking System Events through `osascript` puts up an Automation
/// prompt.
pub fn frontmost_bundle_id(runner: &impl Runner) -> Option<String> {
    let lsappinfo = Path::new(LSAPPINFO);
    let front = runner.run(lsappinfo, &["front"]).ok()?;
    let asn = front.stdout.trim();
    if !front.success() || !asn.starts_with("ASN:") {
        return None;
    }
    let info = runner
        .run(lsappinfo, &["info", "-only", "bundleid", asn])
        .ok()?;
    if !info.success() {
        return None;
    }
    parse_bundle_id(&info.stdout)
}

/// `"CFBundleIdentifier"="com.mitchellh.ghostty"` gives the id (macOS 26).
/// macOS 27 prints the whole info block instead, with only the asked-for
/// line filled in, so the id is on a `bundleID="com.mitchellh.ghostty"` line.
/// An app that has quit, or an ASN that never existed, gives
/// `"CFBundleIdentifier"=[ NULL ]` on macOS 26 and nothing at all on 27.
pub fn parse_bundle_id(stdout: &str) -> Option<String> {
    let value = stdout.lines().find_map(|line| {
        let line = line.trim();
        line.strip_prefix("\"CFBundleIdentifier\"=")
            .or_else(|| line.strip_prefix("bundleID="))
    })?;
    let id = value.trim().strip_prefix('"')?.strip_suffix('"')?;
    (!id.is_empty() && !id.contains('"')).then(|| id.to_owned())
}
