//! Herdr queries, replayed from `tests/fixtures/cli/`, and the socket focus
//! call, against a fake socket that answers with `tests/fixtures/socket/`.

mod support;

use std::os::unix::net::UnixListener;
use std::path::Path;
use std::thread;
use std::time::Duration;

use herdr_nudge::herdr::{self, Cli, Error};
use support::{Recorded, Replay, SocketExchange, fake_herdr, scratch_dir};

const HERDR: &str = "/Users/dev/.local/bin/herdr";

fn cli(replay: &Replay) -> Cli<'_, Replay> {
    Cli {
        bin: Path::new(HERDR),
        runner: replay,
    }
}

#[test]
fn pane_get_reads_focused() {
    let replay = Replay::new([
        Recorded::cli("pane-get-focused"),
        Recorded::cli("pane-get-unfocused"),
    ]);

    let focused = cli(&replay).pane_get("w3:p1").unwrap();
    assert!(focused.focused, "pane-get-focused: focused");
    assert_eq!(focused.workspace_id, "w3");
    assert_eq!(focused.tab_id.as_deref(), Some("w3:t1"));
    assert_eq!(focused.agent.as_deref(), Some("claude"));
    // The spinner the raw terminal_title starts with is already gone.
    let title = focused
        .terminal_title_stripped
        .as_deref()
        .unwrap_or_default();
    assert!(
        title.ends_with("setup and fixture categorization"),
        "pane-get-focused: terminal_title_stripped was {title:?}"
    );

    let shell = cli(&replay).pane_get("w3:p2").unwrap();
    assert!(!shell.focused, "pane-get-unfocused: focused");
    assert_eq!(
        shell.agent, None,
        "pane-get-unfocused: plain shell has no agent"
    );

    assert_eq!(
        *replay.calls.borrow(),
        [
            vec![HERDR, "pane", "get", "w3:p1"],
            vec![HERDR, "pane", "get", "w3:p2"]
        ]
    );
}

/// Herdr's error object comes back as an `Api` error, code intact. The
/// capture is from `agent get`, the only query we have an error reply for.
#[test]
fn pane_get_reports_herdr_errors() {
    let replay = Replay::answering(
        "herdr",
        &["pane", "get", "w3:p2"],
        &Recorded::cli("agent-get-plain-shell"),
    );
    match cli(&replay).pane_get("w3:p2") {
        Err(Error::Api { code, .. }) => assert_eq!(code, "agent_not_found"),
        other => panic!("expected an Api error, got {other:?}"),
    }
}

#[test]
fn a_failed_run_is_an_io_error() {
    let replay = Replay::new([]);
    assert!(matches!(cli(&replay).pane_get("w3:p1"), Err(Error::Io(_))));
}

#[test]
fn plugin_config_dir_is_the_path_herdr_prints() {
    let replay = Replay::new([Recorded::cli("plugin-config-dir")]);
    let dir = cli(&replay).plugin_config_dir("herdr-nudge").unwrap();
    assert_eq!(
        dir,
        Path::new("/Users/dev/.config/herdr/plugins/config/herdr-nudge"),
        "plugin-config-dir: stdout without its newline"
    );
}

#[test]
fn agent_manifests_lists_every_agent_label() {
    let replay = Replay::new([Recorded::cli("agent-manifests")]);
    let labels = cli(&replay).agent_manifests().unwrap();

    assert_eq!(labels.len(), 21, "agent-manifests: label count");
    for label in ["claude", "codex", "pi", "amp", "muse"] {
        assert!(
            labels.iter().any(|l| l == label),
            "agent-manifests: {label}"
        );
    }
    assert_eq!(
        *replay.calls.borrow(),
        [vec![HERDR, "server", "agent-manifests", "--json"]]
    );
}

#[test]
fn focus_pane_sends_pane_focus_over_the_socket() {
    let exchange = SocketExchange::load("pane-focus-ok");
    let (path, server) = fake_herdr("focus_ok", &exchange);

    herdr::focus_pane(&path, "w3:p1", Duration::from_secs(2)).unwrap();

    let sent: serde_json::Value = serde_json::from_str(&server.join().unwrap().request).unwrap();
    let captured: serde_json::Value = serde_json::from_str(&exchange.request).unwrap();
    assert_eq!(sent["method"], captured["method"]);
    assert_eq!(sent["params"], captured["params"]);
}

#[test]
fn focus_pane_reports_a_missing_pane() {
    let exchange = SocketExchange::load("pane-focus-not-found");
    let (path, server) = fake_herdr("focus_missing", &exchange);

    match herdr::focus_pane(&path, "w999:p1", Duration::from_secs(2)) {
        Err(Error::Api { code, .. }) => assert_eq!(code, "pane_not_found"),
        other => panic!("pane-focus-not-found: expected an Api error, got {other:?}"),
    }
    server.join().unwrap();
}

/// The pane id goes through the JSON encoder, so quotes can't change the
/// request's shape.
#[test]
fn focus_pane_encodes_the_pane_id() {
    let exchange = SocketExchange::load("pane-focus-not-found");
    let (path, server) = fake_herdr("focus_encoded", &exchange);

    let odd = "w1:p1\",\"method\":\"server.stop";
    let _ = herdr::focus_pane(&path, odd, Duration::from_secs(2));

    let sent: serde_json::Value = serde_json::from_str(&server.join().unwrap().request).unwrap();
    assert_eq!(sent["method"], "pane.focus");
    assert_eq!(sent["params"]["pane_id"], odd);
}

#[test]
fn focus_pane_with_no_socket_is_an_io_error() {
    let path = scratch_dir("focus_pane_with_no_socket_is_an_io_error").join("absent.sock");
    assert!(matches!(
        herdr::focus_pane(&path, "w3:p1", Duration::from_secs(1)),
        Err(Error::Io(_))
    ));
}

#[test]
fn focus_pane_gives_up_on_a_silent_socket() {
    let path = scratch_dir("silent_socket").join("herdr.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        thread::sleep(Duration::from_millis(500));
        drop(stream);
    });

    let result = herdr::focus_pane(&path, "w3:p1", Duration::from_millis(100));
    assert!(matches!(result, Err(Error::Io(_))), "{result:?}");
    server.join().unwrap();
}

/// The server's own version, not the binary's: after `herdr update` the old
/// server runs on until a restart.
#[test]
fn server_version_reads_the_running_servers_version() {
    for (capture, want) in [
        ("status-server", (0, 9, 0)),
        ("status-server-0.9.2", (0, 9, 2)),
    ] {
        let replay = Replay::new([Recorded::cli(capture)]);
        assert_eq!(cli(&replay).server_version().unwrap(), want, "{capture}");
    }
}

#[test]
fn versions_parse_up_to_their_third_number() {
    assert_eq!(herdr::parse_version("0.9.2"), Some((0, 9, 2)));
    assert_eq!(herdr::parse_version("0.9.3-rc.1"), Some((0, 9, 3)));
    assert_eq!(herdr::parse_version("1.0.0+build"), Some((1, 0, 0)));
    assert_eq!(herdr::parse_version("0.9"), None);
    assert_eq!(herdr::parse_version("v0.9.2"), None);
}

/// The title goes through the JSON encoder, so one starting with `-` can't
/// pass for a flag, and the reply says why nothing was shown.
#[test]
fn show_notification_sends_the_title_body_and_done_sound() {
    let exchange = SocketExchange::load("notification-show-0.9.2");
    let (path, server) = fake_herdr("show_note", &exchange);

    let reply = herdr::show_notification(
        &path,
        "sleep 8 · done",
        "herdr-nudge",
        Duration::from_secs(2),
    )
    .unwrap();
    assert_eq!(
        reply,
        herdr::Shown {
            shown: false,
            reason: "disabled".to_owned()
        },
        "notification-show-0.9.2: no client attached and toasts off"
    );

    let sent: serde_json::Value = serde_json::from_str(&server.join().unwrap().request).unwrap();
    let captured: serde_json::Value = serde_json::from_str(&exchange.request).unwrap();
    assert_eq!(sent["method"], captured["method"]);
    assert_eq!(sent["params"], captured["params"]);
}
