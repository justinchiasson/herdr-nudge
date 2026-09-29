//! One event, end to end, with Herdr and the notifier replaced by recordings.

mod support;

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use herdr_nudge::classify::{ClassifySignal, PaneKind};
use herdr_nudge::cli::JobId;
use herdr_nudge::config::Config;
use herdr_nudge::event::{AgentStatus, Envelope, EventData};
use herdr_nudge::handler::{self, Deps, Outcome};
use herdr_nudge::process::Spawner;
use herdr_nudge::state::{AgentsCache, Loaded, StateDir, now_ms};
use support::{Fixture, Recorded, Replay, SocketExchange, Spy, fake_herdr, scratch_dir};

/// A state directory, a fake plugin with a fake notifier in it, and a `herdr`
/// that only answers what it was given.
struct Harness {
    state: StateDir,
    /// Must be named `herdr`: the replay matches a recording by the
    /// program's file name.
    herdr_bin: PathBuf,
    config: Config,
    runner: Replay,
    spy: Spy,
    plugin_root: PathBuf,
    notifier_bin: PathBuf,
    self_bin: PathBuf,
    socket: PathBuf,
    now_ms: u64,
}

impl Harness {
    fn new(test_name: &str, recordings: Vec<Recorded>) -> Harness {
        let dir = scratch_dir(test_name);
        let plugin_root = dir.join("plugin");
        let notifier_bin = herdr_nudge::notifier::binary_path(&plugin_root);
        fs::create_dir_all(notifier_bin.parent().expect("bundle dir")).expect("bundle dir");
        fs::write(&notifier_bin, b"#!/bin/sh\n").expect("fake notifier");
        // Absolute, because the click command refuses anything else.
        let self_bin = plugin_root.join("bin/herdr-nudge");
        fs::create_dir_all(self_bin.parent().expect("bin dir")).expect("bin dir");
        fs::write(&self_bin, b"#!/bin/sh\n").expect("fake binary");

        Harness {
            state: StateDir::new(dir.join("state")),
            herdr_bin: dir.join("herdr"),
            config: Config::default(),
            runner: Replay::new(recordings),
            spy: Spy::default(),
            plugin_root,
            notifier_bin,
            self_bin,
            socket: dir.join("herdr.sock"),
            now_ms: now_ms(),
        }
    }

    /// `herdr pane get <pane>` answered from a recording, whatever pane the
    /// recording was captured against.
    fn answering(test_name: &str, pane_id: &str, recording: &str) -> Harness {
        let mut harness = Harness::new(test_name, Vec::new());
        harness.runner = Replay::answering(
            "herdr",
            &["pane", "get", pane_id],
            &Recorded::cli(recording),
        );
        harness
    }

    fn deps(&self) -> Deps<'_, Replay, Spy> {
        self.deps_with(&self.spy)
    }

    fn deps_with<'a, S: Spawner>(&'a self, spawner: &'a S) -> Deps<'a, Replay, S> {
        Deps {
            config: &self.config,
            state: &self.state,
            herdr_bin: &self.herdr_bin,
            socket_path: &self.socket,
            notifier_bin: &self.notifier_bin,
            self_bin: &self.self_bin,
            plugin_root: &self.plugin_root,
            runner: &self.runner,
            spawner,
            now_ms: self.now_ms,
            pid: 4242,
        }
    }

    fn handle(&self, fixture: &str) -> Outcome {
        self.handle_envelope(&Fixture::load(fixture).envelope())
    }

    fn handle_envelope(&self, envelope: &Envelope) -> Outcome {
        handler::handle(&self.deps(), envelope, None).outcome
    }

    fn report(&self, fixture: &str) -> handler::Report {
        self.report_envelope(&Fixture::load(fixture).envelope())
    }

    fn report_envelope(&self, envelope: &Envelope) -> handler::Report {
        handler::handle(&self.deps(), envelope, None)
    }

    fn remember_agents(&self, labels: &[&str]) {
        let cache = AgentsCache::new(labels.iter().map(|l| l.to_string()), self.now_ms);
        self.state.save_agents_cache(&cache).expect("save cache");
    }
}

/// The cheap gate: a status no trigger set names must not cost a subprocess.
#[test]
fn a_working_event_asks_herdr_nothing() {
    let harness = Harness::new("working_asks_nothing", Vec::new());
    let outcome = harness.handle("agent/working");
    assert_eq!(
        outcome,
        Outcome::StatusNotWatched(AgentStatus::Working),
        "agent/working should stop before any query"
    );
    assert_eq!(
        harness.runner.call_count(),
        0,
        "agent/working ran a subprocess"
    );
    assert!(
        harness.spy.spawns.borrow().is_empty(),
        "agent/working started the notifier"
    );
}

#[test]
fn an_unknown_status_is_watched_by_nobody() {
    let harness = Harness::new("unknown_status", Vec::new());
    assert_eq!(
        harness.handle("agent/status-unknown-no-agent-field"),
        Outcome::StatusNotWatched(AgentStatus::Unknown),
        "an unknown status is not a trigger"
    );
    assert_eq!(
        harness.runner.call_count(),
        0,
        "unknown status queried Herdr"
    );
}

/// A pane Herdr detects by itself is an agent, and `blocked` notifies.
#[test]
fn a_blocked_agent_posts_a_notification() {
    let harness = Harness::answering("blocked_posts", "w1:p1", "pane-get-unfocused");
    harness.remember_agents(&["claude"]);

    let Outcome::Posted(posted) = harness.handle("agent/blocked") else {
        panic!(
            "agent/blocked did not post: {:?}",
            harness.handle("agent/blocked")
        );
    };
    assert_eq!(posted.kind, PaneKind::Agent, "agent/blocked pane kind");
    assert_eq!(
        posted.signal,
        Some(ClassifySignal::Catalogue),
        "claude is in the remembered manifests"
    );
    assert_eq!(posted.group, "herdr-nudge-w1:p1", "agent/blocked group");
    assert_eq!(posted.title, "Claude · blocked", "agent/blocked title");

    let argv = harness.spy.only();
    assert_eq!(
        argv[0],
        harness.notifier_bin.display().to_string(),
        "the bundled notifier was started"
    );
    assert_eq!(
        Spy::arg_after(&argv, "-execute"),
        Some(format!(
            "'{}' --click {}",
            harness.self_bin.display(),
            posted.job_id
        )),
        "the -execute value"
    );
}

/// Herdr plays its own sound for a `blocked`, so by default we add none.
#[test]
fn the_banner_is_silent_unless_sound_is_on() {
    let harness = Harness::answering("silent_by_default", "w1:p1", "pane-get-unfocused");
    harness.remember_agents(&["claude"]);
    assert!(matches!(
        harness.handle("agent/blocked"),
        Outcome::Posted(_)
    ));
    let argv = harness.spy.only();
    assert!(
        !argv.iter().any(|a| a == "-sound"),
        "agent/blocked with the default config: {argv:?}"
    );

    let mut harness = Harness::answering("sound_on", "w1:p1", "pane-get-unfocused");
    harness.config.notifications.sound = true;
    harness.remember_agents(&["claude"]);
    assert!(matches!(
        harness.handle("agent/blocked"),
        Outcome::Posted(_)
    ));
    assert_eq!(
        Spy::arg_after(&harness.spy.only(), "-sound").as_deref(),
        Some("default"),
        "agent/blocked with sound = true"
    );
}

/// A harness for the zsh `done` from the 0.9.2 capture, on a server whose
/// `status server` answers from `capture`.
fn shell_done_on(test_name: &str, capture: &str) -> Harness {
    let mut harness = Harness::new(test_name, Vec::new());
    harness.runner = Replay::answering(
        "herdr",
        &["pane", "get", "wQ:p2"],
        &Recorded::cli("pane-get-unfocused"),
    )
    .with(Recorded::cli(capture));
    harness.remember_agents(&["claude"]);
    harness
}

fn asked_version(harness: &Harness) -> bool {
    harness
        .runner
        .calls
        .borrow()
        .iter()
        .any(|call| call[1..].starts_with(&["status".to_owned(), "server".to_owned()]))
}

/// Herdr 0.9.2 releases a finished shell command within a second, and then
/// shows no toast and plays no sound for it, so we ask it to.
#[test]
fn a_shell_done_on_herdr_0_9_2_asks_herdr_for_its_sound() {
    let mut harness = shell_done_on("shell_sound_0_9_2", "status-server-0.9.2");
    let exchange = SocketExchange::load("notification-show-0.9.2");
    let (socket, server) = fake_herdr("sound_sock", &exchange);
    harness.socket = socket;

    let Outcome::Posted(posted) = harness.handle("shell/done-unwatched-failed-0.9.2") else {
        panic!("shell/done-unwatched-failed-0.9.2 did not post");
    };
    let sent: serde_json::Value = serde_json::from_str(&server.join().unwrap().request).unwrap();
    assert_eq!(sent["method"], "notification.show");
    assert_eq!(sent["params"]["title"], posted.title.as_str());
    assert_eq!(sent["params"]["sound"], "done");
    let event = Fixture::load("shell/done-unwatched-failed-0.9.2").envelope();
    let EventData::PaneAgentStatusChanged(event) = event.data else {
        panic!("expected a status event");
    };
    assert_eq!(sent["params"]["body"], event.title.unwrap().as_str());

    let argv = harness.spy.only();
    assert!(
        !argv.iter().any(|a| a == "-sound"),
        "the banner's own sound would be a second one: {argv:?}"
    );
}

/// Up to 0.9.1 the pane stays `done` until the next command, so Herdr does
/// it all itself.
#[test]
fn a_shell_done_on_herdr_0_9_0_leaves_it_to_herdr() {
    let harness = shell_done_on("shell_sound_0_9_0", "status-server");
    let report = harness.report("shell/done-unwatched-failed-0.9.2");
    assert!(matches!(report.outcome, Outcome::Posted(_)), "{report:?}");
    // Nothing listens on this harness's socket, so asking would leave one
    // of these notes.
    assert!(
        !report
            .notes
            .iter()
            .any(|n| n.contains("for its sound") || n.starts_with("herdr showed nothing")),
        "asked for Herdr's sound on the 0.9.0 server in status-server: {:?}",
        report.notes
    );
    assert!(
        !only_job(&harness).released_by_herdr,
        "status-server is 0.9.0, which keeps the pane until the next command"
    );
}

/// The one job on disk.
fn only_job(harness: &Harness) -> herdr_nudge::state::Job {
    let ids = harness.state.job_ids().unwrap();
    assert_eq!(ids.len(), 1, "{ids:?}");
    match harness.state.job(&ids[0]).unwrap() {
        Loaded::Found(job) => job,
        other => panic!("job {}: {other:?}", ids[0]),
    }
}

/// An unknown version is treated as 0.9.2 or later. On an older server
/// that costs a second sound. The other way, a newer server's banner would
/// vanish as soon as it went up.
#[test]
fn a_server_version_that_cant_be_read_counts_as_0_9_2() {
    let harness = Harness::answering("version_unknown", "wQ:p2", "pane-get-unfocused");
    harness.remember_agents(&["claude"]);
    let report = harness.report("shell/done-unwatched-failed-0.9.2");
    assert!(matches!(report.outcome, Outcome::Posted(_)), "{report:?}");
    assert!(
        only_job(&harness).released_by_herdr,
        "released_by_herdr for shell/done-unwatched-failed-0.9.2 with no version"
    );
    assert!(
        report
            .notes
            .iter()
            .any(|n| n.starts_with("could not ask the server's version")),
        "{:?}",
        report.notes
    );
}

/// A reporter that sends `idle` with no `working` first, as herdr-ohmyzsh
/// does below its threshold, gets an `idle` banner
/// (`shell/idle-without-working`). 0.9.2 releases that one too, and Herdr
/// plays nothing for an `idle`, so neither do we.
#[test]
fn herdrs_own_release_after_a_shell_idle_leaves_the_banner_up() {
    let harness = shell_done_on("released_after_idle", "status-server-0.9.2");
    let report = handler::handle(
        &harness.deps(),
        &on_pane("shell/idle-with-title-labels", "wQ:p2"),
        None,
    );
    assert!(matches!(report.outcome, Outcome::Posted(_)), "{report:?}");
    assert!(
        !report.notes.iter().any(|n| n.contains("for its sound")),
        "asked for Herdr's sound for an idle: {:?}",
        report.notes
    );

    let release = Fixture::load("shell/released-by-herdr-after-done-0.9.2")
        .event_json_with("agent", "make".into());
    harness.handle_envelope(&Envelope::parse(&release).unwrap());
    assert_eq!(
        removes(&harness.spy),
        Vec::<String>::new(),
        "Herdr's release of make after shell/idle-with-title-labels"
    );
}

/// Herdr takes one of these a second and does nothing without a client.
/// Either way the banner is up and the reason goes to the log.
#[test]
fn herdr_showing_nothing_is_only_a_note() {
    let mut harness = shell_done_on("shell_sound_nothing", "status-server-0.9.2");
    let exchange = SocketExchange::load("notification-show-0.9.2");
    let (socket, server) = fake_herdr("nothing_sock", &exchange);
    harness.socket = socket;
    let report = harness.report("shell/done-unwatched-failed-0.9.2");
    server.join().unwrap();
    assert!(matches!(report.outcome, Outcome::Posted(_)), "{report:?}");
    assert!(
        report
            .notes
            .contains(&"herdr showed nothing: disabled".to_owned()),
        "notification-show-0.9.2 answers disabled: {:?}",
        report.notes
    );
}

/// Agents keep Herdr's own sound, so posting one costs no version query.
#[test]
fn an_agent_done_does_not_ask_the_servers_version() {
    let harness = Harness::answering("agent_no_version", "w1:p1", "pane-get-unfocused");
    harness.remember_agents(&["claude"]);
    let outcome = harness.handle_envelope(&on_pane("agent/done", "w1:p1"));
    assert!(matches!(outcome, Outcome::Posted(_)), "{outcome:?}");
    let argv = harness.spy.only();
    assert!(!argv.iter().any(|a| a == "-sound"), "{argv:?}");
    assert!(!asked_version(&harness), "asked the version for an agent");
}

/// Everything the click needs has to be on disk before the banner is up,
/// because the click gets no environment at all.
#[test]
fn the_job_file_carries_what_the_click_cannot_look_up() {
    let mut harness = Harness::answering("job_file", "w1:p1", "pane-get-unfocused");
    harness.config.default_terminal = Some("com.mitchellh.ghostty".to_owned());
    harness.remember_agents(&["claude"]);

    let Outcome::Posted(posted) = harness.handle("agent/blocked") else {
        panic!("agent/blocked did not post");
    };
    let Ok(Loaded::Found(job)) = harness.state.job(&posted.job_id) else {
        panic!("no job file for {}", posted.job_id);
    };

    assert_eq!(job.pane_id, "w1:p1", "job pane_id");
    assert_eq!(job.workspace_id, "w1", "job workspace_id");
    assert_eq!(
        job.agent_label.as_deref(),
        Some("claude"),
        "job agent_label"
    );
    assert_eq!(job.kind, PaneKind::Agent, "job kind");
    assert_eq!(job.status, AgentStatus::Blocked, "job status");
    assert_eq!(
        job.bundle_id.as_deref(),
        Some("com.mitchellh.ghostty"),
        "job bundle_id, from default_terminal"
    );
    assert_eq!(job.socket_path, harness.socket, "job socket_path");
    assert_eq!(job.notifier_path, harness.notifier_bin, "job notifier_path");
    assert!(
        job.expires_at_ms > job.created_at_ms,
        "job expiry {} is not after creation {}",
        job.expires_at_ms,
        job.created_at_ms
    );
}

/// Reporting metadata alone emits a status event carrying the unchanged
/// status, so the same status arrives more than once.
#[test]
fn the_same_status_twice_posts_once() {
    let harness = Harness::answering("dedup", "w1:p1", "pane-get-unfocused");
    harness.remember_agents(&["claude"]);

    let first = harness.handle("agent/blocked");
    assert!(matches!(first, Outcome::Posted(_)), "first: {first:?}");

    let second = harness.handle("agent/blocked");
    let Outcome::AlreadyShowing(job_id) = &second else {
        panic!("second agent/blocked should have been suppressed: {second:?}");
    };
    if let Outcome::Posted(posted) = first {
        assert_eq!(
            *job_id,
            posted.job_id.to_string(),
            "the live job should be the one already posted"
        );
    }
    assert_eq!(
        harness.spy.spawns.borrow().len(),
        1,
        "the notifier should have run once"
    );
}

/// A different status for the same pane is a different thing to say.
#[test]
fn blocked_then_done_posts_twice() {
    let harness = Harness::answering("blocked_then_done", "w1:p1", "pane-get-unfocused");
    harness.remember_agents(&["claude"]);

    assert!(matches!(
        harness.handle("agent/blocked"),
        Outcome::Posted(_)
    ));
    let second = harness.handle("agent/done");
    assert!(
        matches!(second, Outcome::Posted(_)),
        "agent/done after blocked: {second:?}"
    );
    assert_eq!(
        harness.spy.spawns.borrow().len(),
        2,
        "both statuses should post"
    );
}

/// If deleting the superseded `blocked` job failed, it is still on disk when
/// the pane goes `blocked` again. The `done` banner is what's showing, so
/// the new `blocked` must post rather than match the leftover.
#[test]
fn a_leftover_older_job_does_not_suppress_a_repeat() {
    let mut harness = Harness::answering("leftover_job", "w1:p1", "pane-get-unfocused");
    harness.remember_agents(&["claude"]);

    let Outcome::Posted(blocked) = harness.handle("agent/blocked") else {
        panic!("agent/blocked did not post");
    };
    let Ok(Loaded::Found(leftover)) = harness.state.job(&blocked.job_id) else {
        panic!("no job file for agent/blocked");
    };
    harness.now_ms += 1_000;
    assert!(matches!(harness.handle("agent/done"), Outcome::Posted(_)));
    // Put the blocked job back, as if its delete had failed.
    harness
        .state
        .save_job(&leftover)
        .expect("restore leftover job");

    harness.now_ms += 1_000;
    let again = harness.handle("agent/blocked");
    assert!(
        matches!(again, Outcome::Posted(_)),
        "agent/blocked after done, with the old blocked job left over: {again:?}"
    );
}

/// Focused pane plus the bound terminal in front means the user is looking
/// at it.
#[test]
fn a_pane_the_user_is_watching_stays_quiet() {
    let mut harness = Harness::new("watching", Vec::new());
    let mut recordings = vec![
        Recorded::sys("lsappinfo-front"),
        Recorded::sys("lsappinfo-bundleid-ghostty"),
    ];
    let mut pane_get = Recorded::cli("pane-get-focused");
    pane_get.argv = ["herdr", "pane", "get", "w1:p1"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    recordings.push(pane_get);
    harness.runner = Replay::new(recordings);
    harness.config.default_terminal = Some("com.mitchellh.ghostty".to_owned());
    harness.remember_agents(&["claude"]);

    assert_eq!(
        harness.handle("agent/blocked"),
        Outcome::Watching,
        "focused pane in the frontmost terminal should not notify"
    );
    assert!(
        harness.spy.spawns.borrow().is_empty(),
        "nothing should have been posted"
    );
}

/// `pane get` for `w1:p1` answered from `recording`, then the captured
/// `nudge-capture` session, whose socket the harness now claims: clients in
/// Ghostty (used last) and iTerm.
/// `tests/terminal.rs` describes that capture.
fn with_two_terminals(test_name: &str, recording: &str, front: &str) -> Harness {
    let mut pane_get = Recorded::cli(recording);
    pane_get.argv = ["herdr", "pane", "get", "w1:p1"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let mut frontmost = Recorded::sys("lsappinfo-bundleid-ghostty");
    frontmost.stdout = format!("\"CFBundleIdentifier\"=\"{front}\"\n");
    let mut harness = Harness::new(test_name, Vec::new());
    harness.runner = Replay::new([
        pane_get,
        Recorded::pgrep_herdr("pgrep-herdr-two-sessions"),
        Recorded::sys("lsof-capture-session-clients"),
        Recorded::sys("ps-env-capture-session-clients"),
        Recorded::sys("lsappinfo-visible-process-list"),
        Recorded::sys("lsappinfo-find-ghostty"),
        Recorded::sys("lsappinfo-find-iterm"),
        Recorded::sys("lsappinfo-front"),
        frontmost,
    ]);
    harness.socket = PathBuf::from("/Users/dev/.config/herdr/sessions/nudge-capture/herdr.sock");
    harness.remember_agents(&["claude"]);
    harness
}

/// With no `default_terminal`, the attached clients' terminals are what
/// counts as "the user is looking". Either one: iTerm in front shows the
/// focused pane as much as Ghostty does, though Ghostty is what a click
/// raises.
#[test]
fn any_client_terminal_in_front_counts_as_watching() {
    for front in ["com.mitchellh.ghostty", "com.googlecode.iterm2"] {
        let harness = with_two_terminals("client_watching", "pane-get-focused", front);
        assert_eq!(
            harness.handle("agent/blocked"),
            Outcome::Watching,
            "pane-get-focused with {front} in front"
        );
    }
    let harness = with_two_terminals(
        "client_not_watching",
        "pane-get-focused",
        "com.apple.Safari",
    );
    assert!(
        matches!(harness.handle("agent/blocked"), Outcome::Posted(_)),
        "pane-get-focused with Safari in front"
    );
}

#[test]
fn the_job_names_the_client_terminal_and_asks_the_click_to_look_again() {
    let harness = with_two_terminals("client_job", "pane-get-unfocused", "com.mitchellh.ghostty");
    let Outcome::Posted(posted) = harness.handle("agent/blocked") else {
        panic!("agent/blocked on an unfocused pane did not post");
    };
    let Ok(Loaded::Found(job)) = harness.state.job(&posted.job_id) else {
        panic!("no job file");
    };
    assert_eq!(
        job.bundle_id.as_deref(),
        Some("com.mitchellh.ghostty"),
        "job bundle_id, the client terminal used last"
    );
    assert!(job.detect_at_click, "job detect_at_click");
}

/// The config wins at click time as well, so the click is given nothing to
/// look again with.
#[test]
fn default_terminal_leaves_the_click_nothing_to_detect() {
    let mut harness = Harness::answering("config_job", "w1:p1", "pane-get-unfocused");
    harness.config.default_terminal = Some("com.googlecode.iterm2".to_owned());
    harness.remember_agents(&["claude"]);
    let Outcome::Posted(posted) = harness.handle("agent/blocked") else {
        panic!("agent/blocked on an unfocused pane did not post");
    };
    let Ok(Loaded::Found(job)) = harness.state.job(&posted.job_id) else {
        panic!("no job file");
    };
    assert_eq!(job.bundle_id.as_deref(), Some("com.googlecode.iterm2"));
    assert!(!job.detect_at_click);
    assert_eq!(
        harness.runner.call_count(),
        1,
        "only pane get: {:?}",
        harness.runner.calls.borrow()
    );
}

/// A repeat of what is already showing costs no subprocess at all.
#[test]
fn a_repeat_is_caught_before_asking_anything() {
    let mut harness = with_two_terminals(
        "client_repeat",
        "pane-get-unfocused",
        "com.mitchellh.ghostty",
    );
    assert!(matches!(
        harness.handle("agent/blocked"),
        Outcome::Posted(_)
    ));
    let before = harness.runner.call_count();
    harness.now_ms += 1_000;
    assert!(matches!(
        harness.handle("agent/blocked"),
        Outcome::AlreadyShowing(_)
    ));
    assert_eq!(
        harness.runner.call_count(),
        before,
        "the repeat ran: {:?}",
        &harness.runner.calls.borrow()[before..]
    );
}

/// A pane the user is on, but some other app is in front: notify.
#[test]
fn a_focused_pane_behind_another_app_still_notifies() {
    let mut harness = Harness::new("focused_but_behind", Vec::new());
    let mut pane_get = Recorded::cli("pane-get-focused");
    pane_get.argv = ["herdr", "pane", "get", "w1:p1"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let mut front = Recorded::sys("lsappinfo-bundleid-ghostty");
    front.stdout = "\"CFBundleIdentifier\"=\"com.apple.Safari\"\n".to_owned();
    harness.runner = Replay::new(vec![pane_get, Recorded::sys("lsappinfo-front"), front]);
    harness.config.default_terminal = Some("com.mitchellh.ghostty".to_owned());
    harness.remember_agents(&["claude"]);

    let outcome = harness.handle("agent/blocked");
    assert!(
        matches!(outcome, Outcome::Posted(_)),
        "a browser in front should not suppress: {outcome:?}"
    );
}

/// When the query fails we know neither whether the user is watching nor
/// what the pane is. A spurious notification beats a missed one.
#[test]
fn a_failed_pane_query_still_notifies() {
    let harness = Harness::new("query_fails", Vec::new());
    let outcome = harness.handle("shell/done-unwatched-failed");
    assert!(
        matches!(outcome, Outcome::Posted(_)),
        "an unanswered pane get should not silence us: {outcome:?}"
    );
}

#[test]
fn a_shell_command_is_classified_and_titled_as_one() {
    let harness = Harness::answering("shell_classified", "w3:p3", "pane-get-unfocused");
    harness.remember_agents(&["claude", "codex"]);

    let Outcome::Posted(posted) = harness.handle("shell/done-unwatched-failed") else {
        panic!("shell/done-unwatched-failed did not post");
    };
    assert_eq!(posted.kind, PaneKind::Shell, "make is not a known agent");
    assert_eq!(
        posted.signal,
        Some(ClassifySignal::Neither),
        "no catalogue entry and no session"
    );
    assert_eq!(posted.title, "make · failed", "shell title");
}

/// The command name is the label a shell reporter sends, `make` in this
/// capture. An agent pane with the same label is not affected.
#[test]
fn an_ignored_command_is_dropped_for_shell_panes_only() {
    let mut harness = Harness::answering("ignored_command", "w3:p3", "pane-get-unfocused");
    harness.config.shell.ignore_commands = vec!["make".to_owned()];
    assert_eq!(
        harness.handle("shell/done-unwatched-failed"),
        Outcome::IgnoredCommand("make".to_owned()),
        "shell/done-unwatched-failed: make is in ignore_commands"
    );

    let mut harness = Harness::answering("ignored_command_agent", "w1:p1", "pane-get-unfocused");
    harness.config.shell.ignore_commands = vec!["claude".to_owned()];
    harness.remember_agents(&["claude"]);
    let outcome = harness.handle("agent/blocked");
    assert!(
        matches!(outcome, Outcome::Posted(_)),
        "agent/blocked: ignore_commands must not mute an agent: {outcome:?}"
    );
}

/// Keyed on the agent label, like `known_agents_extra`. A shell command
/// with the same name is not affected.
#[test]
fn an_ignored_agent_is_dropped_for_agent_panes_only() {
    let mut harness = Harness::answering("ignored_agent", "w1:p1", "pane-get-unfocused");
    harness.config.agents.ignore = vec!["claude".to_owned()];
    harness.remember_agents(&["claude"]);
    assert_eq!(
        harness.handle("agent/blocked"),
        Outcome::IgnoredAgent("claude".to_owned()),
        "agent/blocked: claude is in [agents] ignore"
    );

    let mut harness = Harness::answering("ignored_agent_shell", "w3:p3", "pane-get-unfocused");
    harness.config.agents.ignore = vec!["make".to_owned()];
    let outcome = harness.handle("shell/done-unwatched-failed");
    assert!(
        matches!(outcome, Outcome::Posted(_)),
        "shell/done-unwatched-failed: [agents] ignore must not mute a shell command: {outcome:?}"
    );
}

#[test]
fn notify_on_failure_only_drops_a_command_that_worked() {
    let mut harness = Harness::answering("failure_only", "w3:p4", "pane-get-unfocused");
    harness.config.shell.notify_on_failure_only = true;

    assert_eq!(
        harness.handle("shell/done-unwatched-after-handover"),
        Outcome::NotAFailure,
        "state_labels[idle] is \"done\", not \"failed\""
    );
}

#[test]
fn notify_on_failure_only_keeps_a_command_that_failed() {
    let mut harness = Harness::answering("failure_only_keeps", "w3:p3", "pane-get-unfocused");
    harness.config.shell.notify_on_failure_only = true;

    let outcome = harness.handle("shell/done-unwatched-failed");
    assert!(
        matches!(outcome, Outcome::Posted(_)),
        "state_labels[idle] is \"failed\": {outcome:?}"
    );
}

#[test]
fn a_disabled_half_never_notifies() {
    let mut harness = Harness::answering("agents_disabled", "w1:p1", "pane-get-unfocused");
    harness.config.agents.enabled = false;
    harness.remember_agents(&["claude"]);

    // The union gate sees `blocked` is still wanted by the shell side, so the
    // query happens; classification then lands on the disabled half.
    assert_eq!(
        harness.handle("agent/blocked"),
        Outcome::StatusNotWatched(AgentStatus::Blocked),
        "with agents off, blocked is in neither enabled trigger set"
    );
}

#[test]
fn a_missing_notifier_is_reported_not_ignored() {
    let harness = Harness::answering("no_notifier", "w1:p1", "pane-get-unfocused");
    harness.remember_agents(&["claude"]);
    fs::remove_file(&harness.notifier_bin).expect("remove the fake notifier");

    assert_eq!(
        harness.handle("agent/blocked"),
        Outcome::NotifierMissing(harness.notifier_bin.clone()),
        "a missing bundle should say so"
    );
}

#[test]
fn an_expired_job_is_swept_and_its_banner_withdrawn() {
    let mut harness = Harness::answering("sweep", "w1:p1", "pane-get-unfocused");
    harness.config.notifications.clickable_secs = 0;
    harness.remember_agents(&["claude"]);

    let Outcome::Posted(posted) = harness.handle("agent/blocked") else {
        panic!("agent/blocked did not post");
    };
    harness.spy.spawns.borrow_mut().clear();

    let live = handler::live_jobs(
        &harness.state,
        &harness.spy,
        harness.now_ms + 1,
        &mut Vec::new(),
    );
    assert!(live.is_empty(), "an expired job came back as live");
    assert_eq!(
        Spy::arg_after(&harness.spy.only(), "-remove").as_deref(),
        Some("herdr-nudge-w1:p1"),
        "the group should be withdrawn"
    );
    assert!(
        matches!(harness.state.job(&posted.job_id), Ok(Loaded::Missing)),
        "the job file should be gone"
    );
}

#[test]
fn a_live_job_is_left_alone_by_the_sweep() {
    let harness = Harness::answering("sweep_keeps", "w1:p1", "pane-get-unfocused");
    harness.remember_agents(&["claude"]);
    let Outcome::Posted(posted) = harness.handle("agent/blocked") else {
        panic!("agent/blocked did not post");
    };
    harness.spy.spawns.borrow_mut().clear();

    let live = handler::live_jobs(
        &harness.state,
        &harness.spy,
        harness.now_ms,
        &mut Vec::new(),
    );
    assert_eq!(live.len(), 1, "the live job");
    assert!(
        harness.spy.spawns.borrow().is_empty(),
        "withdrew a job that has not expired"
    );
    assert!(
        matches!(harness.state.job(&posted.job_id), Ok(Loaded::Found(_))),
        "the job file should still be there"
    );
}

/// We subscribe to three events. Anything else Herdr sends does nothing.
#[test]
fn events_we_do_not_subscribe_to_are_not_handled() {
    let harness = Harness::new("other_events", Vec::new());
    for fixture in [
        "focus/tab-focus-tab-focused",
        "focus/manual-tab-click-workspace-focused",
        "detected/shell-claim",
        "lifecycle/pane-created",
    ] {
        assert_eq!(
            harness.handle(fixture),
            Outcome::NotHandled,
            "{fixture} should do nothing"
        );
    }
}

/// No event should ever make the hook panic, whatever it carries.
#[test]
fn every_captured_event_is_handled_without_panicking() {
    let harness = Harness::new("all_fixtures", Vec::new());
    for fixture in Fixture::all() {
        let outcome = harness.handle_envelope(&fixture.envelope());
        assert!(
            !matches!(outcome, Outcome::Failed(_)),
            "{}: {outcome:?}",
            fixture.name
        );
    }
}

/// A label that would climb out of the logos directory is not used as a path.
#[test]
fn a_strange_agent_label_gets_no_logo() {
    let harness = Harness::answering("strange_label", "w1:p1", "pane-get-unfocused");
    let fixture = Fixture::load("agent/blocked");
    let json = fixture.event_json_with("agent", "../../../etc/passwd".into());
    let envelope = Envelope::parse(&json).expect("mutated fixture parses");
    harness.remember_agents(&["../../../etc/passwd"]);

    let outcome = harness.handle_envelope(&envelope);
    assert!(
        matches!(outcome, Outcome::Posted(_)),
        "should still post: {outcome:?}"
    );
    let argv = harness.spy.only();
    assert!(
        Spy::arg_after(&argv, "-contentImage").is_none(),
        "a label with path segments must not become an image path: {argv:?}"
    );
}

/// Puts `assets/agents/<label>.png`, and `dark/<label>.png` if asked, in the
/// harness's plugin, and returns both paths.
fn install_logo(harness: &Harness, label: &str, with_dark: bool) -> (PathBuf, PathBuf) {
    let icons = harness.plugin_root.join(handler::LOGO_DIR);
    fs::create_dir_all(icons.join("dark")).expect("logos dir");
    let light = icons.join(format!("{label}.png"));
    let dark = icons.join("dark").join(format!("{label}.png"));
    fs::write(&light, b"light").expect("light logo");
    if with_dark {
        fs::write(&dark, b"dark").expect("dark logo");
    }
    (light, dark)
}

fn posted_logo(harness: &Harness) -> Option<String> {
    assert!(
        matches!(harness.handle("agent/blocked"), Outcome::Posted(_)),
        "agent/blocked should post"
    );
    Spy::arg_after(&harness.spy.only(), "-contentImage")
}

fn asked_appearance(harness: &Harness) -> bool {
    harness
        .runner
        .calls
        .borrow()
        .iter()
        .any(|call| call[0].ends_with("/defaults"))
}

#[test]
fn an_agent_with_no_logo_file_posts_without_one() {
    let harness = Harness::answering("no_logo_file", "w1:p1", "pane-get-unfocused");
    harness.remember_agents(&["claude"]);
    assert_eq!(posted_logo(&harness), None, "no assets/agents/claude.png");
    assert!(
        !asked_appearance(&harness),
        "asked for the appearance with no logo"
    );
}

#[test]
fn a_logo_with_no_dark_copy_is_used_without_asking_the_appearance() {
    let harness = Harness::answering("logo_light_only", "w1:p1", "pane-get-unfocused");
    harness.remember_agents(&["claude"]);
    let (light, _) = install_logo(&harness, "claude", false);
    assert_eq!(
        posted_logo(&harness),
        Some(light.display().to_string()),
        "-contentImage for claude with one file"
    );
    assert!(
        !asked_appearance(&harness),
        "ran defaults for a logo that has no dark copy"
    );
}

#[test]
fn a_logo_with_a_dark_copy_follows_light_mode() {
    let mut harness = Harness::answering("logo_light_mode", "w1:p1", "pane-get-unfocused");
    harness.runner =
        std::mem::take(&mut harness.runner).with(Recorded::sys("defaults-appearance-light"));
    harness.remember_agents(&["claude"]);
    let (light, _) = install_logo(&harness, "claude", true);
    assert_eq!(
        posted_logo(&harness),
        Some(light.display().to_string()),
        "sys/defaults-appearance-light should pick assets/agents/claude.png"
    );
    assert!(asked_appearance(&harness), "never asked for the appearance");
}

#[test]
fn a_logo_with_a_dark_copy_follows_dark_mode() {
    let mut harness = Harness::answering("logo_dark_mode", "w1:p1", "pane-get-unfocused");
    harness.runner =
        std::mem::take(&mut harness.runner).with(Recorded::sys("defaults-appearance-dark"));
    harness.remember_agents(&["claude"]);
    let (_, dark) = install_logo(&harness, "claude", true);
    assert_eq!(
        posted_logo(&harness),
        Some(dark.display().to_string()),
        "sys/defaults-appearance-dark should pick assets/agents/dark/claude.png"
    );
}

/// If `defaults` can't be run, the light file still goes up.
#[test]
fn a_failed_appearance_query_uses_the_light_logo() {
    let harness = Harness::answering("logo_no_defaults", "w1:p1", "pane-get-unfocused");
    harness.remember_agents(&["claude"]);
    let (light, _) = install_logo(&harness, "claude", true);
    assert_eq!(
        posted_logo(&harness),
        Some(light.display().to_string()),
        "-contentImage when defaults has no recording"
    );
}

#[test]
fn job_ids_are_sixteen_hex_and_differ_between_events() {
    let mut seen = BTreeSet::new();
    // Same clock, same pid: only the counter separates these two.
    for ms in [1_700_000_000_000u64, 1_700_000_000_000, 1_700_000_000_001] {
        let id = herdr_nudge::state::new_job_id(ms, 4242);
        assert_eq!(id.as_str().len(), 16, "job id length: {id}");
        assert!(JobId::parse(id.as_str()).is_ok(), "job id shape: {id}");
        assert!(seen.insert(id.to_string()), "duplicate job id: {id}");
    }
}

/// An expired job is swept before the repeat check, so it can't swallow
/// the same status arriving again.
#[test]
fn an_expired_job_does_not_block_a_repeat() {
    let mut harness = Harness::answering("expired_repeat", "w1:p1", "pane-get-unfocused");
    harness.config.notifications.clickable_secs = 0;
    harness.remember_agents(&["claude"]);
    assert!(matches!(
        harness.handle("agent/blocked"),
        Outcome::Posted(_)
    ));

    harness.now_ms += 1;
    let again = harness.handle("agent/blocked");
    assert!(
        matches!(again, Outcome::Posted(_)),
        "agent/blocked after its job expired: {again:?}"
    );
}

/// A pane that hands over from an agent to a shell reporter has a new agent
/// identity. The old job still has to go: the group is per-pane, so a sweep
/// of it would later withdraw the notification the new job owns.
#[test]
fn a_handover_leaves_one_job_for_the_pane() {
    let mut harness = Harness::new("handover_one_job", Vec::new());
    let mut pane_get = Recorded::cli("pane-get-unfocused");
    pane_get.argv = ["herdr", "pane", "get", "w1:p1"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    harness.runner = Replay::new(vec![pane_get]);
    harness.remember_agents(&["claude"]);

    let Outcome::Posted(agent_job) = harness.handle("agent/blocked") else {
        panic!("agent/blocked did not post");
    };

    // The same pane, now reporting as `make`: a different identity.
    harness.now_ms += 1_000;
    let shell = Fixture::load("shell/done-unwatched-failed");
    let json = shell.event_json_with("pane_id", "w1:p1".into());
    let envelope = Envelope::parse(&json).expect("retargeted fixture parses");
    let Outcome::Posted(shell_job) = harness.handle_envelope(&envelope) else {
        panic!("the shell report did not post");
    };

    assert_ne!(agent_job.job_id, shell_job.job_id, "a new job was posted");
    assert_eq!(
        harness.state.job_ids().unwrap(),
        vec![shell_job.job_id.clone()],
        "the agent's job should have gone with the handover"
    );
    assert_eq!(
        agent_job.group, shell_job.group,
        "both notifications share the pane's group, which is why the old job had to go"
    );
    assert_eq!(
        removes(&harness.spy),
        Vec::<String>::new(),
        "the new banner replaced the old one, so nothing should be withdrawn"
    );
}

/// If the notifier never starts there is nothing on screen, so the job must
/// not linger and pretend there is.
#[test]
fn a_notification_that_could_not_be_posted_leaves_no_job() {
    let mut harness = Harness::answering("post_fails", "w1:p1", "pane-get-unfocused");
    harness.spy = Spy::failing();
    harness.remember_agents(&["claude"]);

    let outcome = harness.handle("agent/blocked");
    assert!(
        matches!(outcome, Outcome::Failed(_)),
        "a failed spawn should be reported: {outcome:?}"
    );
    assert_eq!(
        harness.state.job_ids().unwrap(),
        Vec::new(),
        "no job should be left for a notification that never appeared"
    );
}

/// The dedup rule must not be primed by a notification that failed to post.
#[test]
fn a_failed_post_does_not_suppress_the_next_event() {
    let mut harness = Harness::answering("post_fails_twice", "w1:p1", "pane-get-unfocused");
    harness.spy = Spy::failing();
    harness.remember_agents(&["claude"]);
    assert!(matches!(
        harness.handle("agent/blocked"),
        Outcome::Failed(_)
    ));

    harness.spy = Spy::default();
    let second = harness.handle("agent/blocked");
    assert!(
        matches!(second, Outcome::Posted(_)),
        "the retry should post rather than dedupe: {second:?}"
    );
}

/// The groups of every `-remove` the notifier was started with, in order.
fn removes(spy: &Spy) -> Vec<String> {
    spy.spawns
        .borrow()
        .iter()
        .filter_map(|argv| Spy::arg_after(argv, "-remove"))
        .collect()
}

fn posts(spy: &Spy) -> usize {
    spy.spawns
        .borrow()
        .iter()
        .filter(|argv| argv.iter().any(|a| a == "-execute"))
        .count()
}

/// A captured event moved to another pane.
fn on_pane(fixture: &str, pane_id: &str) -> Envelope {
    let json = Fixture::load(fixture).event_json_with("pane_id", pane_id.into());
    Envelope::parse(&json).expect("retargeted fixture parses")
}

/// `pane get <pane_id>` answered from `recording`, plus whatever else.
fn pane_get(pane_id: &str, recording: &str) -> Recorded {
    let mut pane_get = Recorded::cli(recording);
    pane_get.argv = ["herdr", "pane", "get", pane_id]
        .iter()
        .map(|s| s.to_string())
        .collect();
    pane_get
}

/// `lsappinfo` saying `bundle_id` is in front.
fn front(bundle_id: &str) -> [Recorded; 2] {
    let mut info = Recorded::sys("lsappinfo-bundleid-ghostty");
    info.stdout = format!("\"CFBundleIdentifier\"=\"{bundle_id}\"\n");
    [Recorded::sys("lsappinfo-front"), info]
}

/// A Claude `blocked` banner up for `pane_id`, with Ghostty as the terminal.
fn blocked_on(test_name: &str, pane_id: &str) -> Harness {
    let mut harness = Harness::answering(test_name, pane_id, "pane-get-unfocused");
    harness.config.default_terminal = Some("com.mitchellh.ghostty".to_owned());
    harness.remember_agents(&["claude"]);
    let outcome = harness.handle_envelope(&on_pane("agent/blocked", pane_id));
    assert!(
        matches!(outcome, Outcome::Posted(_)),
        "agent/blocked on {pane_id}: {outcome:?}"
    );
    harness.now_ms += 1_000;
    harness
}

/// The case that used to be swallowed: the user answers a `blocked` prompt
/// in the terminal, the agent works, and then blocks again. The second
/// `blocked` is new and has to notify.
#[test]
fn a_status_after_the_pane_moved_on_posts_again() {
    let mut harness = blocked_on("blocked_working_blocked", "w1:p1");

    assert_eq!(
        harness.handle("agent/working"),
        Outcome::StatusNotWatched(AgentStatus::Working),
        "agent/working"
    );
    assert_eq!(
        removes(&harness.spy),
        vec!["herdr-nudge-w1:p1"],
        "agent/working should withdraw the blocked banner"
    );
    assert_eq!(
        harness.state.job_ids().unwrap(),
        Vec::new(),
        "agent/working should delete the blocked job"
    );

    harness.now_ms += 1_000;
    let again = harness.handle("agent/blocked");
    assert!(
        matches!(again, Outcome::Posted(_)),
        "agent/blocked after working: {again:?}"
    );
    assert_eq!(posts(&harness.spy), 2, "two blocked banners");
}

/// `working` arrives many times a minute, so taking a banner down on it
/// must not ask Herdr anything.
#[test]
fn moving_on_withdraws_without_a_query() {
    let harness = blocked_on("withdraw_no_query", "w1:p1");
    let before = harness.runner.call_count();
    harness.handle("agent/working");
    assert_eq!(
        harness.runner.call_count(),
        before,
        "agent/working ran: {:?}",
        &harness.runner.calls.borrow()[before..]
    );
}

#[test]
fn a_status_on_another_pane_leaves_the_banner_alone() {
    let harness = blocked_on("other_pane_status", "w1:p1");
    harness.handle_envelope(&on_pane("agent/working", "w1:p9"));
    assert_eq!(
        removes(&harness.spy),
        Vec::<String>::new(),
        "agent/working on w1:p9 withdrew w1:p1's banner"
    );
    assert_eq!(harness.state.job_ids().unwrap().len(), 1, "w1:p1's job");
}

/// `done` replaces `blocked` in the same group. A `-remove` as well could
/// reach the notifier after the post and take the new banner down.
#[test]
fn a_new_banner_replaces_the_old_one_without_a_remove() {
    let harness = blocked_on("blocked_then_done_replace", "w1:p1");
    let Outcome::Posted(done) = harness.handle("agent/done") else {
        panic!("agent/done after blocked did not post");
    };
    assert_eq!(
        removes(&harness.spy),
        Vec::<String>::new(),
        "agent/done should replace, not withdraw"
    );
    assert_eq!(
        harness.state.job_ids().unwrap(),
        vec![done.job_id],
        "only the done job should be left"
    );
}

/// The agent exited. The event after a release has no `agent` field.
#[test]
fn a_release_withdraws_the_agents_banner() {
    let harness = blocked_on("release_withdraws", "w1:p1");
    let outcome = harness.handle_envelope(&on_pane("agent/status-unknown-no-agent-field", "w1:p1"));
    assert_eq!(outcome, Outcome::StatusNotWatched(AgentStatus::Unknown));
    assert_eq!(removes(&harness.spy), vec!["herdr-nudge-w1:p1"]);
    assert_eq!(harness.state.job_ids().unwrap(), Vec::new());
}

/// A zsh `done` banner up for `wQ:p2`, from the 0.9.2 capture.
fn shell_done_on_0_9_2(test_name: &str) -> (Harness, JobId) {
    shell_done_posted(test_name, "status-server-0.9.2")
}

/// The zsh `done` from the 0.9.2 capture, posted on a server whose
/// `status server` answers from `capture`. Nothing listens on the
/// harness's socket, so asking Herdr for its sound only leaves a note.
fn shell_done_posted(test_name: &str, capture: &str) -> (Harness, JobId) {
    let mut harness = shell_done_on(test_name, capture);
    let Outcome::Posted(posted) = harness.handle("shell/done-unwatched-failed-0.9.2") else {
        panic!("shell/done-unwatched-failed-0.9.2 did not post");
    };
    harness.now_ms += 1_000;
    (harness, posted.job_id)
}

/// On 0.9.0 and 0.9.1 a release comes from the reporter when the next
/// command starts. herdr-ohmyzsh and the like send nothing after it, so it
/// has to take the banner down itself.
#[test]
fn a_release_on_herdr_0_9_0_withdraws_the_banner() {
    let (harness, _) = shell_done_posted("released_0_9_0", "status-server");
    harness.handle("shell/released-by-herdr-after-done-0.9.2");
    assert_eq!(
        removes(&harness.spy),
        vec!["herdr-nudge-wQ:p2"],
        "a release on the 0.9.0 server in status-server"
    );
    assert_eq!(harness.state.job_ids().unwrap(), Vec::new());
}

/// Herdr 0.9.2 releases the command itself once the prompt is back, in the
/// same second as the `done`. That isn't the user coming back.
#[test]
fn herdrs_own_release_after_a_shell_done_leaves_the_banner_up() {
    let (harness, job_id) = shell_done_on_0_9_2("released_after_done");
    assert_eq!(
        harness.handle("shell/released-by-herdr-after-done-0.9.2"),
        Outcome::StatusNotWatched(AgentStatus::Unknown)
    );
    assert_eq!(
        removes(&harness.spy),
        Vec::<String>::new(),
        "shell/released-by-herdr-after-done-0.9.2 took the banner down"
    );
    assert_eq!(harness.state.job_ids().unwrap(), vec![job_id]);
}

/// The zsh hook clears the title when the next command starts, which is
/// what takes the banner down now that the release doesn't.
#[test]
fn the_next_command_after_a_shell_done_withdraws_the_banner() {
    let (harness, _) = shell_done_on_0_9_2("cleared_after_done");
    harness.handle("shell/released-by-herdr-after-done-0.9.2");
    let outcome = harness.handle_envelope(&on_pane("shell/cleared-on-next-command-0.9.2", "wQ:p2"));
    assert_eq!(outcome, Outcome::StatusNotWatched(AgentStatus::Unknown));
    assert_eq!(
        removes(&harness.spy),
        vec!["herdr-nudge-wQ:p2"],
        "shell/cleared-on-next-command-0.9.2 should withdraw"
    );
    assert_eq!(
        harness.state.job_ids().unwrap(),
        Vec::new(),
        "job left after shell/cleared-on-next-command-0.9.2"
    );
}

/// Another label being released means something else had the pane since
/// the banner went up.
#[test]
fn a_release_of_another_label_withdraws_the_banner() {
    let (harness, _) = shell_done_on_0_9_2("released_other_label");
    let json = Fixture::load("shell/released-by-herdr-after-done-0.9.2")
        .event_json_with("agent", "make".into());
    harness.handle_envelope(&Envelope::parse(&json).unwrap());
    assert_eq!(
        removes(&harness.spy),
        vec!["herdr-nudge-wQ:p2"],
        "a release of make after the mark.sh banner"
    );
}

/// `known_agents_remove` puts an agent down as a shell command, but Herdr
/// still knows it as an agent, doesn't release it, and plays its own sound.
#[test]
fn an_agent_put_down_as_a_shell_command_keeps_herdrs_own_sound() {
    let mut harness = shell_done_on("agent_as_shell", "status-server-0.9.2");
    harness.config.shell.known_agents_remove = vec!["claude".to_owned()];
    let report = harness.report_envelope(&on_pane("agent/done", "wQ:p2"));
    let Outcome::Posted(posted) = &report.outcome else {
        panic!("agent/done with claude in known_agents_remove: {report:?}");
    };
    assert_eq!(posted.kind, PaneKind::Shell, "known_agents_remove");
    assert!(
        !only_job(&harness).released_by_herdr,
        "released_by_herdr for claude, which Herdr knows"
    );
    assert!(
        !asked_version(&harness),
        "asked the version for a label Herdr knows"
    );
}

/// Herdr lists `omp` as an integration but ships no manifest for it, so
/// it isn't in the manifests. Herdr still knows it and doesn't release it.
#[test]
fn an_agent_without_a_manifest_is_not_released_by_herdr() {
    let mut harness = shell_done_on("omp_as_shell", "status-server-0.9.2");
    harness.config.shell.known_agents_remove = vec!["omp".to_owned()];
    let json =
        Fixture::load("shell/done-unwatched-failed-0.9.2").event_json_with("agent", "omp".into());
    let report = harness.report_envelope(&Envelope::parse(&json).unwrap());
    assert!(matches!(report.outcome, Outcome::Posted(_)), "{report:?}");
    assert!(
        !only_job(&harness).released_by_herdr,
        "released_by_herdr for omp, which Herdr knows"
    );
}

/// A custom agent Herdr doesn't know is released too, once its pane is back
/// at a prompt, as a one-shot run is right after its `done`. Its banner has
/// to stay. It gets no sound from us: an agent that keeps running isn't
/// released, and then Herdr plays its own.
#[test]
fn a_custom_agent_herdr_releases_keeps_its_banner_without_our_sound() {
    let mut harness = shell_done_on("custom_agent", "status-server-0.9.2");
    harness.config.shell.known_agents_extra = vec!["mark.sh".to_owned()];
    let report = harness.report("shell/done-unwatched-failed-0.9.2");
    let Outcome::Posted(posted) = &report.outcome else {
        panic!("mark.sh in known_agents_extra: {report:?}");
    };
    assert_eq!(posted.kind, PaneKind::Agent, "known_agents_extra");
    assert!(
        only_job(&harness).released_by_herdr,
        "released_by_herdr for mark.sh, which Herdr doesn't know"
    );
    assert!(
        !report.notes.iter().any(|n| n.contains("for its sound")),
        "asked for Herdr's sound for an agent: {:?}",
        report.notes
    );

    harness.handle("shell/released-by-herdr-after-done-0.9.2");
    assert_eq!(
        removes(&harness.spy),
        Vec::<String>::new(),
        "Herdr's release of the mark.sh agent"
    );
}

/// `sound = true` adds the macOS sound to every banner, a shell `done` on
/// 0.9.2 included, and Herdr is still asked for its own.
#[test]
fn sound_on_with_herdr_0_9_2_plays_both() {
    let mut harness = shell_done_on("sound_on_0_9_2", "status-server-0.9.2");
    harness.config.notifications.sound = true;
    let exchange = SocketExchange::load("notification-show-0.9.2");
    let (socket, server) = fake_herdr("both_sock", &exchange);
    harness.socket = socket;
    let outcome = harness.handle("shell/done-unwatched-failed-0.9.2");
    assert!(matches!(outcome, Outcome::Posted(_)), "{outcome:?}");
    assert_eq!(
        Spy::arg_after(&harness.spy.only(), "-sound").as_deref(),
        Some("default"),
        "banner sound with sound = true"
    );
    let sent: serde_json::Value = serde_json::from_str(&server.join().unwrap().request).unwrap();
    assert_eq!(sent["method"], "notification.show");
}

/// A new status that doesn't post still means the old banner is out of date.
#[test]
fn a_status_the_user_is_watching_withdraws_the_old_banner() {
    let mut harness = blocked_on("watching_withdraws", "w1:p1");
    let [lsappinfo_front, info] = front("com.mitchellh.ghostty");
    harness.runner = Replay::new([pane_get("w1:p1", "pane-get-focused"), lsappinfo_front, info]);
    assert_eq!(harness.handle("agent/done"), Outcome::Watching);
    assert_eq!(removes(&harness.spy), vec!["herdr-nudge-w1:p1"]);
    assert_eq!(harness.state.job_ids().unwrap(), Vec::new());
}

/// 0.9.0 closed an agent pane by hand, 0.9.1 by `herdr pane close`. Both
/// send the same `pane.closed`.
#[test]
fn closing_a_pane_withdraws_its_banner() {
    for (fixture, pane_id) in [
        ("lifecycle/pane-closed", "w1:p1"),
        ("lifecycle/pane-closed-by-cli", "w3:pA"),
    ] {
        let harness = blocked_on("close_withdraws", pane_id);
        let before = harness.runner.call_count();
        assert_eq!(
            harness.handle(fixture),
            Outcome::Withdrawn(format!("herdr-nudge-{pane_id}")),
            "{fixture}"
        );
        assert_eq!(
            removes(&harness.spy),
            vec![format!("herdr-nudge-{pane_id}")],
            "{fixture}: -remove"
        );
        assert_eq!(
            harness.state.job_ids().unwrap(),
            Vec::new(),
            "{fixture}: jobs left"
        );
        assert_eq!(
            harness.runner.call_count(),
            before,
            "{fixture} ran something"
        );
    }
}

#[test]
fn closing_a_pane_with_nothing_up_does_nothing() {
    let harness = blocked_on("close_other_pane", "w1:p9");
    assert_eq!(
        harness.handle("lifecycle/pane-closed"),
        Outcome::NothingShowing
    );
    assert_eq!(removes(&harness.spy), Vec::<String>::new());
    assert_eq!(harness.state.job_ids().unwrap().len(), 1, "w1:p9's job");
}

/// A captured event with some fields of `data` changed.
fn retarget(fixture: &str, fields: &[(&str, &str)]) -> Envelope {
    let mut root: serde_json::Value =
        serde_json::from_str(&Fixture::load(fixture).event_json).expect("event_json parses");
    for (key, value) in fields {
        root["data"][*key] = (*value).into();
    }
    Envelope::parse(&root.to_string()).expect("retargeted fixture parses")
}

/// Posts a Claude `blocked` for `pane_id` in `workspace_id` on a harness
/// that already has banners up.
fn also_blocked_on(harness: &mut Harness, pane_id: &str, workspace_id: &str) {
    harness.runner = Replay::answering(
        "herdr",
        &["pane", "get", pane_id],
        &Recorded::cli("pane-get-unfocused"),
    );
    let outcome = harness.handle_envelope(&retarget(
        "agent/blocked",
        &[("pane_id", pane_id), ("workspace_id", workspace_id)],
    ));
    assert!(
        matches!(outcome, Outcome::Posted(_)),
        "agent/blocked on {pane_id}: {outcome:?}"
    );
}

/// Banners on `w1:p9`, which `cli/pane-list` doesn't have, and on `w1:p2`,
/// which it does. `pane list` is answered from that capture.
fn banners_on_a_gone_and_a_live_pane(test_name: &str) -> Harness {
    let mut harness = blocked_on(test_name, "w1:p9");
    also_blocked_on(&mut harness, "w1:p2", "w1");
    harness.runner = Replay::new([Recorded::cli("pane-list")]);
    harness
}

fn job_panes(harness: &Harness) -> Vec<String> {
    let mut panes: Vec<String> = harness
        .state
        .job_ids()
        .unwrap()
        .iter()
        .map(|id| match harness.state.job(id) {
            Ok(Loaded::Found(job)) => job.pane_id,
            other => panic!("job {id}: {other:?}"),
        })
        .collect();
    panes.sort();
    panes
}

/// Herdr sends no `pane.closed` for the panes of a closed tab, by hand or by
/// CLI, on either version, and the event doesn't list them.
#[test]
fn closing_a_tab_withdraws_the_banners_of_panes_that_are_gone() {
    for fixture in [
        "lifecycle/tab-closed",
        "lifecycle/tab-closed-by-cli",
        "lifecycle/tab-closed-by-cli-0.9.0",
    ] {
        let harness = banners_on_a_gone_and_a_live_pane("tab_close_withdraws");
        assert_eq!(
            harness.handle(fixture),
            Outcome::PanesGone(vec!["herdr-nudge-w1:p9".to_owned()]),
            "{fixture}"
        );
        assert_eq!(
            removes(&harness.spy),
            vec!["herdr-nudge-w1:p9"],
            "{fixture}: -remove"
        );
        assert_eq!(job_panes(&harness), vec!["w1:p2"], "{fixture}: jobs left");
        assert_eq!(harness.runner.call_count(), 1, "{fixture}: one pane list");
    }
}

/// A closed workspace takes the jobs posted in it, with no question to
/// Herdr: a pane id carries its workspace. Each capture is retargeted to
/// `w1`, where the banners are.
#[test]
fn closing_a_workspace_withdraws_the_banners_posted_in_it() {
    for fixture in [
        "lifecycle/workspace-closed",
        "lifecycle/workspace-closed-by-cli",
        "lifecycle/workspace-closed-by-cli-0.9.0",
    ] {
        let mut harness = banners_on_a_gone_and_a_live_pane("workspace_close_withdraws");
        also_blocked_on(&mut harness, "w3:p9", "w3");
        harness.runner = Replay::new([]);
        assert_eq!(
            harness.handle_envelope(&retarget(fixture, &[("workspace_id", "w1")])),
            Outcome::PanesGone(vec![
                "herdr-nudge-w1:p2".to_owned(),
                "herdr-nudge-w1:p9".to_owned()
            ]),
            "{fixture}"
        );
        assert_eq!(job_panes(&harness), vec!["w3:p9"], "{fixture}: jobs left");
        assert_eq!(
            harness.runner.call_count(),
            0,
            "{fixture}: ran a subprocess"
        );
    }
}

/// Moving a tab's only pane into another tab of the same workspace closes
/// the tab. The pane keeps its id and is still listed, so its banner stays.
#[test]
fn a_tab_closed_by_moving_its_pane_within_the_workspace_leaves_its_banner() {
    let mut harness = blocked_on("pane_move_keeps", "w1:p2");
    harness.runner = Replay::new([Recorded::cli("pane-list")]);
    assert_eq!(
        harness.handle("lifecycle/tab-closed-by-pane-move"),
        Outcome::NothingShowing
    );
    assert_eq!(removes(&harness.spy), Vec::<String>::new());
    assert_eq!(job_panes(&harness), vec!["w1:p2"]);
}

/// Moving a tab's last pane to another workspace closes the tab and gives
/// the pane a new id (`w1:p4` became `w4:p2` in this capture). The job's
/// id is gone, so is its banner. `cli/pane-list` is from another day, so
/// the job is put on `w1:p9`, which it doesn't list.
#[test]
fn a_tab_closed_by_moving_its_pane_to_another_workspace_withdraws_the_old_id() {
    let mut harness = blocked_on("pane_move_away", "w1:p9");
    harness.runner = Replay::new([Recorded::cli("pane-list")]);
    assert_eq!(
        harness.handle("lifecycle/tab-closed-by-pane-move-0.9.0"),
        Outcome::PanesGone(vec!["herdr-nudge-w1:p9".to_owned()])
    );
    assert_eq!(job_panes(&harness), Vec::<String>::new());
}

#[test]
fn closing_a_tab_with_nothing_up_asks_herdr_nothing() {
    let harness = Harness::new("container_close_nothing_up", Vec::new());
    assert_eq!(
        harness.handle("lifecycle/tab-closed"),
        Outcome::NothingShowing
    );
    assert_eq!(
        harness.handle("lifecycle/workspace-closed"),
        Outcome::NothingShowing
    );
    assert_eq!(harness.runner.call_count(), 0, "ran a subprocess");
}

/// Without a pane list a closed tab has nothing to go by, so its banners
/// stay until they expire.
#[test]
fn a_closed_tab_leaves_banners_when_panes_cant_be_listed() {
    let mut harness = blocked_on("tab_close_no_list", "w1:p9");
    harness.runner = Replay::new([]);
    assert_eq!(
        harness.handle("lifecycle/tab-closed"),
        Outcome::NothingShowing
    );
    assert_eq!(removes(&harness.spy), Vec::<String>::new());
    assert_eq!(job_panes(&harness), vec!["w1:p9"]);
}

/// A second session numbers its panes the same way. Its `w1:p9` closing,
/// getting focus, changing status, or its tab or workspace closing, says
/// nothing about ours, and asks Herdr nothing.
#[test]
fn another_servers_events_leave_our_banner_alone() {
    let other_session = |harness: &mut Harness| {
        harness.socket = harness.socket.with_file_name("other-session.sock");
    };
    let cases: [(&str, Envelope); 5] = [
        ("pane.closed", on_pane("lifecycle/pane-closed", "w1:p9")),
        (
            "pane.focused",
            on_pane("focus/manual-tab-click-pane-focused", "w1:p9"),
        ),
        ("working", on_pane("agent/working", "w1:p9")),
        (
            "tab.closed",
            Fixture::load("lifecycle/tab-closed").envelope(),
        ),
        (
            "workspace.closed",
            retarget("lifecycle/workspace-closed", &[("workspace_id", "w1")]),
        ),
    ];
    for (name, envelope) in cases {
        let mut harness = blocked_on("other_server_events", "w1:p9");
        harness.runner = Replay::new([Recorded::cli("pane-list")]);
        other_session(&mut harness);
        let outcome = harness.handle_envelope(&envelope);
        assert!(
            !matches!(outcome, Outcome::Withdrawn(_) | Outcome::PanesGone(_)),
            "{name}: {outcome:?}"
        );
        assert_eq!(
            removes(&harness.spy),
            Vec::<String>::new(),
            "{name}: -remove"
        );
        assert_eq!(job_panes(&harness), vec!["w1:p9"], "{name}: jobs left");
        assert_eq!(harness.runner.call_count(), 0, "{name}: ran a subprocess");
    }
}

/// The group is the pane id alone, so another session's post for the same
/// id replaces our banner on screen. Our job goes with it, or a later
/// withdrawal for it would take the other session's banner down.
#[test]
fn another_servers_post_for_the_same_pane_id_replaces_our_job() {
    let mut harness = blocked_on("other_server_post", "w1:p9");
    harness.socket = harness.socket.with_file_name("other-session.sock");
    harness.runner = Replay::answering(
        "herdr",
        &["pane", "get", "w1:p9"],
        &Recorded::cli("pane-get-unfocused"),
    );
    assert!(matches!(
        harness.handle_envelope(&on_pane("agent/done", "w1:p9")),
        Outcome::Posted(_)
    ));
    assert_eq!(removes(&harness.spy), Vec::<String>::new());
    let jobs = harness.state.job_ids().unwrap();
    assert_eq!(jobs.len(), 1, "only the new job");
    let Ok(Loaded::Found(job)) = harness.state.job(&jobs[0]) else {
        panic!("job {}", jobs[0]);
    };
    assert_eq!(job.socket_path, harness.socket, "the other session's job");
}

/// 0.9.1 sends `pane.focused` when the user clicks their way to a pane.
#[test]
fn going_to_the_pane_withdraws_its_banner() {
    let mut harness = blocked_on("focus_withdraws", "w3:p9");
    harness.runner = Replay::new(front("com.mitchellh.ghostty"));
    assert_eq!(
        harness.handle("focus/manual-tab-click-pane-focused"),
        Outcome::Withdrawn("herdr-nudge-w3:p9".to_owned())
    );
    assert_eq!(removes(&harness.spy), vec!["herdr-nudge-w3:p9"]);
    assert_eq!(harness.state.job_ids().unwrap(), Vec::new());
}

/// Focus moved while the user is in another app, as a script would do it.
/// Nobody saw the pane, so the banner stays.
#[test]
fn focus_while_another_app_is_in_front_leaves_the_banner() {
    let mut harness = blocked_on("focus_unseen", "w3:p9");
    harness.runner = Replay::new(front("com.apple.Safari"));
    assert_eq!(
        harness.handle("focus/manual-tab-click-pane-focused"),
        Outcome::FocusedUnseen
    );
    assert_eq!(removes(&harness.spy), Vec::<String>::new());
    assert_eq!(harness.state.job_ids().unwrap().len(), 1, "w3:p9's job");
}

/// Without `default_terminal`, any client's terminal in front counts, as it
/// does for `Watching`.
#[test]
fn focus_with_any_client_terminal_in_front_withdraws() {
    let harness = with_two_terminals(
        "focus_client_terminal",
        "pane-get-unfocused",
        "com.googlecode.iterm2",
    );
    assert!(matches!(
        harness.handle("agent/blocked"),
        Outcome::Posted(_)
    ));
    let outcome = harness.handle_envelope(&on_pane("focus/manual-tab-click-pane-focused", "w1:p1"));
    assert_eq!(outcome, Outcome::Withdrawn("herdr-nudge-w1:p1".to_owned()));
}

/// Every focus Herdr sends goes through here, including the one our own
/// click causes, after the click has deleted the job. With nothing up it
/// must cost nothing.
#[test]
fn focus_on_a_pane_with_nothing_up_costs_nothing() {
    let harness = Harness::new("focus_nothing", Vec::new());
    for fixture in [
        "focus/socket-pane-focus-pane-focused",
        "focus/socket-pane-focus-moved-pane-focused",
        "focus/manual-tab-click-pane-focused",
        "focus/manual-pane-click-pane-focused",
        "focus/manual-workspace-click-pane-focused",
    ] {
        assert_eq!(
            harness.handle(fixture),
            Outcome::NothingShowing,
            "{fixture}"
        );
    }
    assert_eq!(
        harness.runner.call_count(),
        0,
        "a focus event ran something"
    );
    assert!(
        harness.spy.spawns.borrow().is_empty(),
        "a focus event started the notifier"
    );
}

/// Installed into a Herdr that is already running, the startup hook hasn't
/// run, so there is no agent list yet. The first event that needs one
/// fetches it, and it is kept for the next.
#[test]
fn a_missing_agent_list_is_fetched_and_kept() {
    let mut harness = Harness::new("lazy_agents", Vec::new());
    harness.runner = Replay::new([
        pane_get("w1:p1", "pane-get-unfocused"),
        Recorded::cli("agent-manifests-0.9.1"),
    ]);
    let Outcome::Posted(posted) = harness.handle("agent/blocked") else {
        panic!("agent/blocked with no agent list did not post");
    };
    assert_eq!(
        posted.signal,
        Some(ClassifySignal::Catalogue),
        "claude is in agent-manifests-0.9.1"
    );
    let Ok(Loaded::Found(cache)) = harness.state.agents_cache() else {
        panic!("the fetched list was not saved");
    };
    assert!(cache.agents.contains("letta"), "0.9.1's list: {cache:?}");

    harness.now_ms += 1_000;
    let before = harness.runner.call_count();
    harness.handle("agent/done");
    let fetches = harness.runner.calls.borrow()[before..]
        .iter()
        .filter(|argv| argv.iter().any(|a| a == "agent-manifests"))
        .count();
    assert_eq!(fetches, 0, "the second event fetched the list again");
}

#[test]
fn cleanup_drops_every_job_and_refreshes_the_agent_list() {
    let mut harness = blocked_on("cleanup", "w1:p1");
    harness.handle_envelope(&on_pane("agent/blocked", "w2:p1"));
    // A job that doesn't parse can't be withdrawn. `read_json` renames it
    // aside, so it stops counting as a job.
    let stray = JobId::parse("00000000000000ff").unwrap();
    fs::write(harness.state.job_path(&stray), b"{").unwrap();
    harness.spy.spawns.borrow_mut().clear();
    harness.runner = Replay::new([Recorded::cli("agent-manifests-0.9.1")]);

    let (notes, fetched) = handler::cleanup(
        &harness.state,
        None,
        Some(&harness.herdr_bin),
        &harness.runner,
        &harness.spy,
        harness.now_ms + 1,
    );

    assert_eq!(harness.state.job_ids().unwrap(), Vec::new(), "{notes:?}");
    let aside = fs::read_dir(harness.state.jobs_dir())
        .unwrap()
        .filter_map(|e| e.ok())
        .any(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("00000000000000ff.json.corrupt-")
        });
    assert!(aside, "the unparseable job should have been renamed aside");
    assert_eq!(
        removes(&harness.spy),
        vec!["herdr-nudge-w1:p1", "herdr-nudge-w2:p1"],
        "{notes:?}"
    );
    let Ok(Loaded::Found(cache)) = harness.state.agents_cache() else {
        panic!("no agent list after cleanup: {notes:?}");
    };
    assert!(cache.agents.contains("letta"), "0.9.1's list: {cache:?}");
    assert_eq!(
        fetched.map(|f| f.agents),
        Some(cache.agents),
        "returned list"
    );
}

#[test]
fn cleanup_without_herdr_still_drops_jobs() {
    let harness = blocked_on("cleanup_no_herdr", "w1:p1");
    let (notes, fetched) = handler::cleanup(
        &harness.state,
        None,
        None,
        &harness.runner,
        &harness.spy,
        harness.now_ms + 1,
    );
    assert_eq!(harness.state.job_ids().unwrap(), Vec::new(), "{notes:?}");
    assert_eq!(removes(&harness.spy), vec!["herdr-nudge-w1:p1"]);
    assert!(fetched.is_none(), "no herdr, so nothing fetched");
}

/// A restored pane's first status event can post while the startup hook is
/// still running. That banner belongs to the new server and has to stay.
#[test]
fn cleanup_keeps_a_job_posted_after_it_started() {
    let harness = blocked_on("cleanup_keeps_new", "w1:p1");
    // blocked_on posted the job, then moved the clock on by a second. So a
    // cleanup that started at that second saw the job posted during it.
    let started = harness.now_ms - 1_000;
    let (notes, _) = handler::cleanup(
        &harness.state,
        None,
        None,
        &harness.runner,
        &harness.spy,
        started,
    );
    assert_eq!(harness.state.job_ids().unwrap().len(), 1, "{notes:?}");
    assert_eq!(removes(&harness.spy), Vec::<String>::new(), "{notes:?}");
}

/// A status event with no `agent` field gets its label from `pane get`, and
/// the job stores that one. The same event again is still a repeat.
#[test]
fn a_repeat_without_its_own_label_is_still_a_repeat() {
    let mut harness =
        Harness::answering("repeat_no_label", "w3:p3", "pane-get-reported-no-session");
    let json = Fixture::load("shell/done-unwatched-failed").event_json_with_null("agent");
    let event = Envelope::parse(&json).expect("fixture with agent nulled parses");

    let Outcome::Posted(posted) = harness.handle_envelope(&event) else {
        panic!("shell/done-unwatched-failed without agent did not post");
    };
    let Ok(Loaded::Found(job)) = harness.state.job(&posted.job_id) else {
        panic!("no job file");
    };
    assert_eq!(
        job.agent_label.as_deref(),
        Some("make"),
        "label from pane get"
    );

    harness.now_ms += 1_000;
    assert_eq!(
        harness.handle_envelope(&event),
        Outcome::AlreadyShowing(posted.job_id.to_string()),
        "the same event again"
    );
}

/// An expired job left over next to a live one for the same pane shares its
/// group. Sweeping it must not take the live banner down.
#[test]
fn sweeping_a_leftover_leaves_the_live_banner_up() {
    let mut harness = blocked_on("sweep_leftover", "w1:p1");
    let ids = harness.state.job_ids().unwrap();
    let Ok(Loaded::Found(mut old)) = harness.state.job(&ids[0]) else {
        panic!("no job for w1:p1");
    };
    old.id = "0000000000000001".to_owned();
    old.created_at_ms -= 10_000;
    old.expires_at_ms = harness.now_ms - 1;
    harness.state.save_job(&old).unwrap();
    harness.spy.spawns.borrow_mut().clear();
    harness.now_ms += 1;

    let live = handler::live_jobs(
        &harness.state,
        &harness.spy,
        harness.now_ms,
        &mut Vec::new(),
    );
    assert_eq!(live.len(), 1, "the live job");
    assert_eq!(
        removes(&harness.spy),
        Vec::<String>::new(),
        "a -remove would take the live banner"
    );
    assert_eq!(
        harness.state.job_ids().unwrap(),
        ids,
        "only the live job left"
    );
}

/// `herdr-nudge test`, run in the pane `cli/pane-get-focused` describes,
/// with the terminal in front: everything that makes an event `Watching`.
fn test_harness(test_name: &str, workspace_get: &str) -> Harness {
    // Asked about w3, whatever workspace the recording was captured against.
    let mut workspace = Recorded::cli(workspace_get);
    workspace.argv = ["herdr", "workspace", "get", "w3"].map(String::from).into();
    let mut harness = Harness::new(
        test_name,
        vec![
            Recorded::cli("pane-get-focused"),
            workspace,
            Recorded::sys("lsappinfo-front"),
            Recorded::sys("lsappinfo-bundleid-ghostty"),
        ],
    );
    harness.config.default_terminal = Some("com.mitchellh.ghostty".to_owned());
    harness
}

#[test]
fn test_posts_for_its_own_pane_though_the_user_is_watching() {
    let harness = test_harness("test_agent", "workspace-get");

    let report = handler::test(&harness.deps(), "w3:p1", PaneKind::Agent);
    let Outcome::Posted(posted) = report.outcome else {
        panic!("test on pane-get-focused did not post: {report:?}");
    };
    assert_eq!(posted.title, "Herdr Nudge · blocked", "test title");
    assert_eq!(posted.kind, PaneKind::Agent, "test kind");
    assert_eq!(posted.signal, None, "test is not classified");

    let argv = harness.spy.only();
    assert_eq!(
        Spy::text_after(&argv, "-subtitle").as_deref(),
        Some("herdr-nudge"),
        "subtitle should be workspace-get's label"
    );
    assert_eq!(
        Spy::arg_after(&argv, "-group").as_deref(),
        Some("herdr-nudge-w3:p1")
    );
    let Ok(Loaded::Found(job)) = harness.state.job(&posted.job_id) else {
        panic!("test wrote no job, so its click would do nothing");
    };
    assert_eq!(job.pane_id, "w3:p1");
    assert_eq!(job.workspace_id, "w3", "workspace from pane-get-focused");
    assert_eq!(job.bundle_id.as_deref(), Some("com.mitchellh.ghostty"));
    assert_eq!(
        Spy::arg_after(&argv, "-execute"),
        Some(format!(
            "'{}' --click {}",
            harness.self_bin.display(),
            posted.job_id
        )),
    );
}

#[test]
fn test_shell_looks_like_a_finished_command() {
    let harness = test_harness("test_shell", "workspace-get");

    let report = handler::test(&harness.deps(), "w3:p1", PaneKind::Shell);
    let Outcome::Posted(posted) = report.outcome else {
        panic!("test --shell did not post: {report:?}");
    };
    assert_eq!(posted.title, "herdr-nudge · done", "test --shell title");
    assert_eq!(
        Spy::text_after(&harness.spy.only(), "-message").as_deref(),
        Some("herdr-nudge test --shell · exit 0 · 0s"),
    );
    let Ok(Loaded::Found(job)) = harness.state.job(&posted.job_id) else {
        panic!("test --shell wrote no job");
    };
    assert_eq!(job.kind, PaneKind::Shell);
    assert_eq!(job.status, AgentStatus::Done);
}

#[test]
fn test_replaces_the_panes_banner() {
    let harness = test_harness("test_replaces", "workspace-get");
    let first = handler::test(&harness.deps(), "w3:p1", PaneKind::Agent);
    let Outcome::Posted(first) = first.outcome else {
        panic!("first test did not post");
    };

    let second = handler::test(&harness.deps(), "w3:p1", PaneKind::Shell);
    let Outcome::Posted(second) = second.outcome else {
        panic!("second test did not post");
    };
    assert_ne!(first.job_id, second.job_id);
    assert_eq!(
        harness.state.job_ids().unwrap(),
        vec![second.job_id],
        "only the second test's job should be left"
    );
    assert_eq!(
        removes(&harness.spy),
        Vec::<String>::new(),
        "same group, so the second post replaces the first on screen"
    );
}

#[test]
fn test_without_a_workspace_label_uses_the_id() {
    let harness = test_harness("test_no_label", "workspace-get-not-found");

    let report = handler::test(&harness.deps(), "w3:p1", PaneKind::Agent);
    assert!(matches!(report.outcome, Outcome::Posted(_)), "{report:?}");
    assert_eq!(
        Spy::text_after(&harness.spy.only(), "-subtitle").as_deref(),
        Some("w3")
    );
    assert!(
        report
            .notes
            .iter()
            .any(|n| n.contains("workspace_not_found")),
        "{:?}",
        report.notes
    );
}

#[test]
fn test_with_show_workspace_off_has_no_subtitle_and_asks_nothing() {
    let mut harness = test_harness("test_no_workspace", "workspace-get");
    harness.config.notifications.show_workspace = false;

    let report = handler::test(&harness.deps(), "w3:p1", PaneKind::Agent);
    assert!(matches!(report.outcome, Outcome::Posted(_)), "{report:?}");
    let argv = harness.spy.only();
    assert!(
        !argv.iter().any(|a| a == "-subtitle"),
        "show_workspace = false still sent -subtitle: {argv:?}"
    );
    assert!(
        !harness
            .runner
            .calls
            .borrow()
            .iter()
            .any(|call| call[1..].starts_with(&["workspace".to_owned()])),
        "show_workspace = false still ran workspace get"
    );
}

#[test]
fn an_event_with_show_workspace_off_has_no_subtitle() {
    let mut harness = Harness::answering("event_no_workspace", "w1:p1", "pane-get-unfocused");
    harness.config.notifications.show_workspace = false;
    harness.remember_agents(&["claude"]);
    assert!(matches!(
        harness.handle("agent/blocked"),
        Outcome::Posted(_)
    ));
    let argv = harness.spy.only();
    assert!(
        !argv.iter().any(|a| a == "-subtitle"),
        "agent/blocked with show_workspace = false: {argv:?}"
    );
}

#[test]
fn test_in_a_pane_herdr_doesnt_know_posts_nothing() {
    let harness = test_harness("test_unknown_pane", "workspace-get");

    let report = handler::test(&harness.deps(), "w9:p9", PaneKind::Agent);
    assert!(
        matches!(report.outcome, Outcome::Failed(ref why) if why.starts_with("pane get w9:p9")),
        "{report:?}"
    );
    assert!(harness.spy.spawns.borrow().is_empty(), "notifier started");
    assert!(
        harness.state.job_ids().unwrap().is_empty(),
        "job left behind"
    );
}

/// `workspace create --label ""` really does give `"label": ""`.
#[test]
fn test_with_an_empty_workspace_label_uses_the_id() {
    let harness = test_harness("test_empty_label", "workspace-get-empty-label");

    let report = handler::test(&harness.deps(), "w3:p1", PaneKind::Agent);
    assert!(matches!(report.outcome, Outcome::Posted(_)), "{report:?}");
    assert_eq!(
        Spy::text_after(&harness.spy.only(), "-subtitle").as_deref(),
        Some("w3"),
        "workspace-get-empty-label: subtitle"
    );
}

/// Gives the harness's fake bundle an `Info.plist`, which the fake from
/// [`Harness::new`] lacks (so no other test registers anything), and
/// `osascript` its recorded answer for that bundle.
fn registrable(harness: &mut Harness) -> PathBuf {
    let bundle = herdr_nudge::notifier::bundle_path(&harness.plugin_root);
    fs::write(bundle.join("Contents/Info.plist"), b"<plist/>").unwrap();
    let mut osascript = Recorded::sys("osascript-register-bundle");
    osascript.argv.truncate(5);
    osascript.argv.push(bundle.display().to_string());
    harness.runner = std::mem::take(&mut harness.runner).with(osascript);
    bundle
}

/// Notes, at each spawn, whether the registration marker was there yet.
struct MarkerAtSpawn {
    marker: PathBuf,
    seen: RefCell<Vec<bool>>,
}

impl MarkerAtSpawn {
    fn new(state: &StateDir) -> MarkerAtSpawn {
        MarkerAtSpawn {
            marker: state.registered_path(),
            seen: RefCell::default(),
        }
    }
}

impl Spawner for MarkerAtSpawn {
    fn spawn(&self, _program: &Path, _args: &[String]) -> io::Result<()> {
        self.seen.borrow_mut().push(self.marker.is_file());
        Ok(())
    }
}

#[test]
fn the_first_post_registers_the_notifier() {
    let mut harness = Harness::answering("post_registers", "w1:p1", "pane-get-unfocused");
    harness.config.default_terminal = Some("com.mitchellh.ghostty".to_owned());
    harness.remember_agents(&["claude"]);
    registrable(&mut harness);
    let spawner = MarkerAtSpawn::new(&harness.state);

    let report = handler::handle(
        &harness.deps_with(&spawner),
        &on_pane("agent/blocked", "w1:p1"),
        None,
    );

    assert!(
        matches!(report.outcome, Outcome::Posted(_)),
        "agent/blocked on w1:p1: {:?}",
        report.outcome
    );
    assert_eq!(
        *spawner.seen.borrow(),
        vec![true],
        "marker present at each spawn; notes: {:?}",
        report.notes
    );
}

/// After an update the copy on disk is new, and the first thing to run it
/// can be a withdrawal of a banner posted before the update.
#[test]
fn a_withdrawal_registers_the_notifier_before_running_it() {
    let mut harness = blocked_on("withdraw_registers", "w1:p1");
    registrable(&mut harness);
    let spawner = MarkerAtSpawn::new(&harness.state);

    let report = handler::handle(
        &harness.deps_with(&spawner),
        &on_pane("lifecycle/pane-closed", "w1:p1"),
        None,
    );

    assert_eq!(
        *spawner.seen.borrow(),
        vec![true],
        "marker present at the -remove spawn; outcome {:?}, notes {:?}",
        report.outcome,
        report.notes
    );
}

#[test]
fn cleanup_registers_before_it_withdraws() {
    let mut harness = blocked_on("cleanup_registers", "w1:p1");
    let bundle = registrable(&mut harness);
    let spawner = MarkerAtSpawn::new(&harness.state);

    let (notes, _) = handler::cleanup(
        &harness.state,
        Some(&bundle),
        None,
        &harness.runner,
        &spawner,
        harness.now_ms + 1,
    );

    assert_eq!(
        *spawner.seen.borrow(),
        vec![true],
        "marker present at the -remove spawn; notes {notes:?}"
    );
}
