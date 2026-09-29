//! Which terminal a click brings forward, found from the Herdr clients
//! attached to the server, and reading the frontmost app from `lsappinfo`.
//!
//! The process captures come from a second Herdr session, `nudge-capture`
//! (server 40823), with three clients started in ptys under a bare
//! environment, so the `ps -E` fixtures hold nothing private: 40822 says
//! Ghostty, 40852 iTerm, 40869 nothing. The default session (server 86129,
//! clients 86128 and 21836) was running at the same time.

mod support;

use std::collections::BTreeSet;
use std::path::Path;

use herdr_nudge::config::Config;
use herdr_nudge::terminal::{
    Client, HERDR_PATTERN, Resolution, TerminalSource, asns, attached_clients, detect,
    frontmost_bundle_id, is_client, is_server, parse_bundle_id, parse_lsof, parse_pgrep, parse_ps,
    pick, resolve,
};
use support::{Recorded, Replay};

const GHOSTTY: &str = "com.mitchellh.ghostty";
const ITERM: &str = "com.googlecode.iterm2";
/// `HERDR_SOCKET_PATH` of each session, as the scrubbed fixtures spell it.
const CAPTURE_SOCKET: &str = "/Users/dev/.config/herdr/sessions/nudge-capture/herdr.sock";
const DEFAULT_SOCKET: &str = "/Users/dev/.config/herdr/herdr.sock";

fn args(line: &str) -> Vec<String> {
    line.split(' ').map(str::to_owned).collect()
}

fn capture() -> &'static Path {
    Path::new(CAPTURE_SOCKET)
}

/// Every process call that finding the clients of `nudge-capture` makes.
fn capture_session() -> Vec<Recorded> {
    vec![
        Recorded::pgrep_herdr("pgrep-herdr-two-sessions"),
        Recorded::sys("lsof-capture-session-clients"),
        Recorded::sys("ps-env-capture-session-clients"),
    ]
}

/// The same, plus what ranking Ghostty against iTerm asks `lsappinfo`.
fn capture_session_ranked() -> Vec<Recorded> {
    let mut recordings = capture_session();
    recordings.extend([
        Recorded::sys("lsappinfo-visible-process-list"),
        Recorded::sys("lsappinfo-find-ghostty"),
        Recorded::sys("lsappinfo-find-iterm"),
    ]);
    recordings
}

fn client(pid: u32, age_secs: u64, bundle_id: Option<&str>) -> Client {
    Client {
        pid,
        age_secs,
        bundle_id: bundle_id.map(str::to_owned),
    }
}

/// As in `ps-env-capture-session-clients`.
fn capture_clients() -> Vec<Client> {
    vec![
        client(40822, 85, Some(GHOSTTY)),
        client(40852, 84, Some(ITERM)),
        client(40869, 82, None),
    ]
}

#[test]
fn default_terminal_wins_without_looking_for_clients() {
    let config = Config::parse(&format!("default_terminal = \"{ITERM}\"\n")).unwrap();
    let runner = Replay::new(capture_session_ranked());
    let mut notes = Vec::new();

    assert_eq!(
        resolve(&config, &runner, capture(), &mut notes),
        Resolution {
            bundle_id: Some(ITERM.to_owned()),
            source: TerminalSource::DefaultConfig,
            showing: BTreeSet::from([ITERM.to_owned()]),
        }
    );
    assert_eq!(
        runner.call_count(),
        0,
        "default_terminal ran {:?}",
        runner.calls.borrow()
    );
}

/// Both servers are left out because they are servers. The default
/// session's clients are left out because none of their sockets connects to
/// the server holding the capture session's socket.
#[test]
fn only_the_clients_of_our_own_server_are_kept() {
    let runner = Replay::new(capture_session());
    let mut notes = Vec::new();
    let clients = attached_clients(&runner, capture(), &mut notes);

    assert_eq!(clients, capture_clients(), "notes: {notes:?}");
    assert!(notes.is_empty(), "{notes:?}");
    assert_eq!(
        *runner.calls.borrow(),
        [
            vec!["/usr/bin/pgrep", "-a", "-lf", HERDR_PATTERN],
            vec![
                "/usr/sbin/lsof",
                "-b",
                "-w",
                "-U",
                "-a",
                "-p",
                "21836,40822,40823,40852,40869,86128,86129",
                "-F",
                "dn"
            ],
            vec![
                "/bin/ps",
                "-Eww",
                "-o",
                "pid=,etime=,command=",
                "-p",
                "40822,40852,40869"
            ],
        ]
    );
}

/// Seen from the default session, its own two clients are the ones kept.
/// Their environments are real, so they weren't captured, and this stops at
/// which pids `ps` is asked about.
#[test]
fn the_default_session_keeps_only_its_own_clients() {
    let runner = Replay::new(capture_session());
    let mut notes = Vec::new();
    let clients = attached_clients(&runner, Path::new(DEFAULT_SOCKET), &mut notes);
    assert_eq!(clients, []);
    assert_eq!(
        runner.calls.borrow().last().map(|c| c[5].as_str()),
        Some("21836,86128"),
        "ps should be asked about the default session's clients"
    );
}

/// One client is checked like several: the only client on the machine can
/// belong to another session, and then ours has none.
#[test]
fn a_single_client_of_another_session_is_not_ours() {
    let mut pgrep = Recorded::pgrep_herdr("pgrep-herdr-two-sessions");
    pgrep.stdout = pgrep
        .stdout
        .lines()
        .filter(|l| l.starts_with("40822 ") || l.starts_with("40823 "))
        .map(|l| format!("{l}\n"))
        .collect();
    let recordings = [
        pgrep,
        Recorded::sys("lsof-capture-session-one-client"),
        Recorded::sys("ps-env-capture-session-one-client"),
    ];

    let runner = Replay::new(recordings.clone());
    let mut notes = Vec::new();
    let (clients, picked) = detect(&runner, capture(), &mut notes);
    assert_eq!(
        clients,
        [client(40822, 85, Some(GHOSTTY))],
        "notes: {notes:?}"
    );
    assert_eq!(picked.as_deref(), Some(GHOSTTY));
    assert_eq!(
        notes,
        ["clients 40822=com.mitchellh.ghostty -> com.mitchellh.ghostty"]
    );

    // No process in lsof-capture-session-one-client holds the default
    // socket, so which server 40822 belongs to can't be settled either way.
    let runner = Replay::new(recordings);
    let mut notes = Vec::new();
    let clients = attached_clients(&runner, Path::new(DEFAULT_SOCKET), &mut notes);
    assert_eq!(clients.len(), 1, "kept, and noted: {notes:?}");
    assert!(notes[0].contains("no process holds"), "{notes:?}");
}

#[test]
fn no_client_at_all_is_unresolved() {
    let runner = Replay::new([Recorded::pgrep_herdr("pgrep-herdr-none")]);
    let mut notes = Vec::new();
    let resolution = resolve(&Config::default(), &runner, capture(), &mut notes);
    assert_eq!(resolution.source, TerminalSource::Unresolved);
    assert!(resolution.showing.is_empty());
    assert_eq!(
        notes,
        ["clients none -> -"],
        "pgrep-herdr-none exits 1, which is not an error"
    );
    assert_eq!(runner.call_count(), 1);
}

/// lsof exits 1 when a pid it was given has exited since pgrep, and still
/// prints the rest.
#[test]
fn lsof_exiting_1_with_output_is_still_read() {
    let mut recordings = capture_session();
    recordings[1].exit_code = 1;
    let runner = Replay::new(recordings);
    let mut notes = Vec::new();
    assert_eq!(
        attached_clients(&runner, capture(), &mut notes),
        capture_clients(),
        "notes: {notes:?}"
    );
    assert!(notes.is_empty(), "{notes:?}");
}

#[test]
fn lsof_with_no_output_keeps_every_candidate() {
    let mut recordings = capture_session();
    recordings[1].exit_code = 1;
    recordings[1].stdout = String::new();
    let mut ps = Recorded::sys("ps-env-capture-session-clients");
    ps.argv = args("ps -Eww -o pid=,etime=,command= -p 21836,40822,40852,40869,86128");
    recordings[2] = ps;
    let runner = Replay::new(recordings);
    let mut notes = Vec::new();

    let clients = attached_clients(&runner, capture(), &mut notes);
    assert_eq!(
        clients,
        capture_clients(),
        "21836 and 86128 kept but not in ps's answer"
    );
    assert!(notes.iter().any(|n| n.contains("lsof exited")), "{notes:?}");
}

/// Ghostty is first in `lsappinfo-visible-process-list` and iTerm third, so
/// Ghostty was used last.
#[test]
fn the_terminal_used_last_is_raised_and_both_count_as_showing() {
    let runner = Replay::new(capture_session_ranked());
    let mut notes = Vec::new();
    assert_eq!(
        resolve(&Config::default(), &runner, capture(), &mut notes),
        Resolution {
            bundle_id: Some(GHOSTTY.to_owned()),
            source: TerminalSource::Client,
            showing: BTreeSet::from([GHOSTTY.to_owned(), ITERM.to_owned()]),
        },
        "notes: {notes:?}"
    );
}

/// A hidden app drops out of `visibleProcessList`.
#[test]
fn a_hidden_terminal_loses_to_a_visible_one() {
    let mut recordings = capture_session_ranked();
    let visible = &mut recordings[3];
    visible.stdout = visible.stdout.replace("ASN:0x0-0x64b64b-\"Ghostty\": ", "");
    assert_eq!(
        pick(
            &Replay::new(recordings),
            &capture_clients(),
            &mut Vec::new()
        )
        .as_deref(),
        Some(ITERM)
    );
}

/// 40869 is the newest client but has no terminal, so 40852 (iTerm) is
/// the newest that does.
#[test]
fn with_every_terminal_hidden_the_newest_client_wins() {
    let mut recordings = capture_session_ranked();
    recordings[3].stdout = "ASN:0x0-0x434434-\"Google_Chrome\":\n".to_owned();
    assert_eq!(
        pick(
            &Replay::new(recordings),
            &capture_clients(),
            &mut Vec::new()
        )
        .as_deref(),
        Some(ITERM)
    );
}

#[test]
fn one_terminal_needs_no_ranking() {
    let runner = Replay::default();
    let clients = [
        client(1, 5, Some(GHOSTTY)),
        client(2, 1, Some(GHOSTTY)),
        client(3, 0, None),
    ];
    assert_eq!(
        pick(&runner, &clients, &mut Vec::new()).as_deref(),
        Some(GHOSTTY)
    );
    assert_eq!(pick(&runner, &[client(3, 0, None)], &mut Vec::new()), None);
    assert_eq!(runner.call_count(), 0);
}

#[test]
fn which_command_lines_are_clients() {
    let listed = parse_pgrep(&Recorded::pgrep_herdr("pgrep-herdr-two-sessions").stdout);
    let clients: Vec<u32> = listed
        .iter()
        .filter(|(_, a)| is_client(a))
        .map(|(pid, _)| *pid)
        .collect();
    assert_eq!(
        clients,
        [21836, 40822, 40852, 40869, 86128],
        "pgrep-herdr-two-sessions"
    );

    assert!(is_client(&args("-herdr")), "iTerm2's custom shell");
    assert!(is_client(&args("herdr session attach work")));
    assert!(!is_client(&args("herdr session list")));
    assert!(
        !is_client(&args("herdr pane get w1:p1")),
        "a hook's own query"
    );
    assert!(
        !is_client(&args("herdr --remote mini")),
        "another machine's server"
    );
    assert!(!is_client(&args("herdr --session x --remote mini")));
    assert!(!is_client(&args("herdr --remote=mini")));
    assert!(!is_client(&args("herdr --version")), "exits at once");
    assert!(is_client(&args("herdr --session=work")));
}

/// Real `pgrep`, with `sleep` standing in under each name. Replays can't
/// check the pattern itself, since they answer whatever it is.
#[test]
fn the_pgrep_pattern_finds_herdr_by_any_name_but_not_herdr_nudge() {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let names = [
        ("herdr", true),
        ("-herdr", true),
        ("/opt/tools/herdr", true),
        ("herdr-nudge", false),
        ("-herdr-nudge", false),
        ("/opt/tools/herdr-nudge", false),
    ];
    let mut children: Vec<_> = names
        .iter()
        .map(|(name, _)| {
            Command::new("/bin/sleep")
                .arg0(name)
                .arg("30")
                .stdout(Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect();
    let out = Command::new("/usr/bin/pgrep")
        .args(["-a", "-lf", HERDR_PATTERN])
        .output()
        .unwrap();
    for child in &mut children {
        let _ = child.kill();
        let _ = child.wait();
    }

    let listed: BTreeSet<u32> = parse_pgrep(&String::from_utf8_lossy(&out.stdout))
        .into_iter()
        .map(|(pid, _)| pid)
        .collect();
    for ((name, found), child) in names.iter().zip(&children) {
        assert_eq!(
            listed.contains(&child.id()),
            *found,
            "pgrep {HERDR_PATTERN:?} on a process named {name:?}"
        );
    }
}

#[test]
fn ps_lines_give_age_and_terminal() {
    assert_eq!(
        parse_ps(
            &Recorded::sys("ps-env-capture-session-clients").stdout,
            |_| Some(3)
        ),
        capture_clients()
    );
    let ages: Vec<u64> = parse_ps(
        "1 21:58:33 herdr\n2 1-00:00:01 herdr\n3 junk herdr\n",
        |_| None,
    )
    .iter()
    .map(|c| c.age_secs)
    .collect();
    assert_eq!(ages, [79_113, 86_401], "a bad etime drops the line");
    assert_eq!(
        parse_ps("1 00:01 herdr __CFBundleIdentifier=a;b\n", |_| Some(1))[0].bundle_id,
        None,
        "an id with odd characters"
    );
    let from_args = "1 00:01 herdr --session __CFBundleIdentifier=x __CFBundleIdentifier=com.a.b\n";
    assert_eq!(
        parse_ps(from_args, |_| Some(3))[0].bundle_id.as_deref(),
        Some("com.a.b"),
        "a match among the arguments is skipped"
    );
}

#[test]
fn lsof_fields_group_by_process_and_file() {
    let sockets = parse_lsof(&Recorded::sys("lsof-capture-session-clients").stdout);
    let iterm: Vec<&str> = sockets
        .iter()
        .filter(|s| s.pid == 40852)
        .map(|s| s.name.as_str())
        .collect();
    assert_eq!(
        iterm[0], "->0x5bcbdf78a622100c",
        "lsof-capture-session-clients, 40852 fd 5"
    );
    assert!(
        sockets.iter().any(|s| s.pid == 40823
            && s.device == "0x5bcbdf78a622100c"
            && s.name.ends_with("/herdr-client.sock")),
        "the server side of 40852's connection"
    );
}

#[test]
fn asns_from_each_lsappinfo_shape() {
    assert_eq!(
        asns(&Recorded::sys("lsappinfo-front").stdout),
        ["0x0-0x64b64b"]
    );
    assert_eq!(
        asns(&Recorded::sys("lsappinfo-find-iterm").stdout),
        ["0x0-0xa20a20"]
    );
    assert!(asns(&Recorded::sys("lsappinfo-find-not-running").stdout).is_empty());
    let visible = asns(&Recorded::sys("lsappinfo-visible-process-list").stdout);
    assert_eq!(
        &visible[..3],
        ["0x0-0x64b64b", "0x0-0x434434", "0x0-0xa20a20"]
    );
}

/// Recorded `lsappinfo front` then `info`, the two calls the visibility
/// check makes.
#[test]
fn the_frontmost_app_comes_from_recorded_lsappinfo() {
    let lsappinfo = Replay::new([
        Recorded::sys("lsappinfo-front"),
        Recorded::sys("lsappinfo-bundleid-ghostty"),
    ]);
    assert_eq!(frontmost_bundle_id(&lsappinfo).as_deref(), Some(GHOSTTY));
    assert_eq!(
        *lsappinfo.calls.borrow(),
        [
            vec!["/usr/bin/lsappinfo", "front"],
            vec![
                "/usr/bin/lsappinfo",
                "info",
                "-only",
                "bundleid",
                "ASN:0x0-0x64b64b:"
            ]
        ]
    );
}

#[test]
fn bundle_id_parsing() {
    assert_eq!(
        parse_bundle_id(&Recorded::sys("lsappinfo-bundleid-ghostty").stdout).as_deref(),
        Some(GHOSTTY)
    );
    assert_eq!(
        parse_bundle_id(&Recorded::sys("lsappinfo-bundleid-gone").stdout),
        None,
        "lsappinfo-bundleid-gone: [ NULL ]"
    );
    assert_eq!(parse_bundle_id(""), None);
}

/// macOS 27 answers `info -only bundleid` with the whole info block, with
/// only the `bundleID=` line filled in, instead of the one line macOS 26
/// prints. Without this the frontmost app is never known there, so a pane
/// you are looking at still notifies.
#[test]
fn the_frontmost_app_is_read_from_the_macos_27_block_too() {
    assert_eq!(
        parse_bundle_id(&Recorded::sys("lsappinfo-bundleid-ghostty-macos27").stdout).as_deref(),
        Some(GHOSTTY)
    );

    // `front` is still one line on macOS 27. These two were captured one
    // after the other, with TextEdit in front.
    let lsappinfo = Replay::new([
        Recorded::sys("lsappinfo-front-macos27"),
        Recorded::sys("lsappinfo-bundleid-textedit-macos27"),
    ]);
    assert_eq!(
        frontmost_bundle_id(&lsappinfo).as_deref(),
        Some("com.apple.TextEdit")
    );

    for gone in [
        "lsappinfo-bundleid-quit-macos27",
        "lsappinfo-bundleid-gone-macos27",
    ] {
        assert_eq!(
            parse_bundle_id(&Recorded::sys(gone).stdout),
            None,
            "{gone}: prints nothing"
        );
    }
}

#[test]
fn a_front_reply_that_is_not_an_asn_asks_nothing_more() {
    let mut front = Recorded::sys("lsappinfo-front");
    front.stdout = String::new();
    let replay = Replay::new([front]);
    assert_eq!(frontmost_bundle_id(&replay), None);
    assert_eq!(replay.call_count(), 1);
}

/// Our own startup hook runs `herdr server agent-manifests`, and a click can
/// land while it does.
#[test]
fn only_a_bare_herdr_server_is_a_server() {
    assert!(is_server(&args("/Users/dev/.local/bin/herdr server")));
    assert!(!is_server(&args("herdr server agent-manifests --json")));
    assert!(!is_server(&args("herdr server stop")));
    assert!(!is_server(&args("herdr")));
}
