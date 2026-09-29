//! What a clicked notification does: clear its job, raise the terminal,
//! focus the pane.

mod support;

use std::cell::RefCell;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Instant;

use herdr_nudge::classify::PaneKind;
use herdr_nudge::cli::JobId;
use herdr_nudge::click::{self, Outcome};
use herdr_nudge::event::AgentStatus;
use herdr_nudge::process::{Output, Runner};
use herdr_nudge::state::{Job, Loaded, ServerStateDir, StateDir, VERSION};
use support::{Recorded, Replay, SocketExchange, Spy, fake_herdr, scratch_dir};

/// A [`Replay`] that also notes when each call was made, so a test can tell
/// whether `open -b` ran before the socket was reached.
struct Timed {
    replay: Replay,
    at: RefCell<Vec<Instant>>,
}

impl Timed {
    fn new(replay: Replay) -> Timed {
        Timed {
            replay,
            at: RefCell::default(),
        }
    }
}

impl Runner for Timed {
    fn run(&self, program: &Path, args: &[&str]) -> io::Result<Output> {
        self.at.borrow_mut().push(Instant::now());
        self.replay.run(program, args)
    }
}

fn opens_ghostty() -> Replay {
    Replay::new([Recorded::sys("open-bundle-ghostty")])
}

fn job_id() -> JobId {
    JobId::parse("0123456789abcdef").expect("16 hex")
}

/// Pane `w3:p1`, the one `tests/fixtures/socket/pane-focus-ok.json` focused.
fn a_job(id: &JobId, expires_at_ms: u64, socket_path: &Path) -> Job {
    Job {
        version: VERSION,
        id: id.to_string(),
        pane_id: "w3:p1".to_owned(),
        workspace_id: "w3".to_owned(),
        agent_label: Some("claude".to_owned()),
        kind: PaneKind::Agent,
        status: AgentStatus::Blocked,
        group: "herdr-nudge-w3:p1".to_owned(),
        bundle_id: Some("com.mitchellh.ghostty".to_owned()),
        detect_at_click: false,
        released_by_herdr: false,
        socket_path: socket_path.to_owned(),
        notifier_path: PathBuf::from(
            "/plugin/vendor/HerdrNudge.app/Contents/MacOS/terminal-notifier",
        ),
        created_at_ms: 1_000,
        expires_at_ms,
    }
}

fn state_for(test_name: &str) -> StateDir {
    StateDir::new(scratch_dir(test_name).join("state"))
}

/// A socket path nothing listens on, for tests that aren't about focusing.
fn no_socket(test_name: &str) -> PathBuf {
    scratch_dir(&format!("{test_name}_sock")).join("absent.sock")
}

/// A notification can sit in Notification Center long after its job is gone.
#[test]
fn clicking_with_no_job_does_nothing() {
    let state = state_for("click_no_job");
    let runner = Replay::default();
    let spy = Spy::default();
    let (outcome, notes) = click::run_with(&state, |_| Vec::new(), &runner, &spy, &job_id(), 2_000);

    assert_eq!(outcome, Outcome::NoJob, "no job file");
    assert!(notes.is_empty(), "nothing worth logging: {notes:?}");
    assert!(
        spy.spawns.borrow().is_empty() && runner.call_count() == 0,
        "nothing should have been run"
    );
}

#[test]
fn clicking_raises_the_terminal_then_focuses_the_pane() {
    let state = state_for("click_focus");
    let exchange = SocketExchange::load("pane-focus-ok");
    let (socket, server) = fake_herdr("ck_ok", &exchange);
    let id = job_id();
    state
        .save_job(&a_job(&id, 9_000, &socket))
        .expect("save job");

    let runner = Timed::new(opens_ghostty());
    let (outcome, notes) = click::run(&state, None, &runner, &Spy::default(), &id, 2_000);

    assert_eq!(
        outcome,
        Outcome::Focused {
            pane_id: "w3:p1".to_owned()
        },
        "notes: {notes:?}"
    );
    assert!(notes.is_empty(), "nothing went wrong: {notes:?}");
    assert_eq!(
        *runner.replay.calls.borrow(),
        [vec!["/usr/bin/open", "-b", "com.mitchellh.ghostty"]],
        "open -b should get the job's bundle id"
    );

    let received = server.join().expect("fake socket");
    let sent: serde_json::Value = serde_json::from_str(&received.request).expect("request json");
    assert_eq!(sent["method"], "pane.focus");
    assert_eq!(sent["params"]["pane_id"], "w3:p1", "the job's pane");
    assert!(
        runner.at.borrow()[0] < received.connected_at,
        "open -b should run before the socket is reached, so the focus change lands in a raised window"
    );
}

/// The banner can be an hour old. Posted while the user was in iTerm, clicked
/// after they moved to Ghostty: Ghostty is raised. Only 40822 (Ghostty) of
/// the `nudge-capture` clients is left attached here, and the fake socket
/// stands in for that session's.
#[test]
fn a_click_raises_the_terminal_attached_now_not_the_one_posted_from() {
    let state = state_for("click_redetect");
    let (socket, server) = fake_herdr("ck_redet", &SocketExchange::load("pane-focus-ok"));
    let id = job_id();
    let mut job = a_job(&id, 9_000, &socket);
    job.bundle_id = Some("com.googlecode.iterm2".to_owned());
    job.detect_at_click = true;
    state.save_job(&job).expect("save job");

    let mut pgrep = Recorded::pgrep_herdr("pgrep-herdr-two-sessions");
    pgrep.stdout = pgrep
        .stdout
        .lines()
        .filter(|l| l.starts_with("40822 ") || l.starts_with("40823 "))
        .map(|l| format!("{l}\n"))
        .collect();
    let mut lsof = Recorded::sys("lsof-capture-session-one-client");
    lsof.stdout = lsof.stdout.replace(
        "/Users/dev/.config/herdr/sessions/nudge-capture/herdr.sock",
        &socket.to_string_lossy(),
    );
    let runner = Replay::new([
        pgrep,
        lsof,
        Recorded::sys("ps-env-capture-session-one-client"),
        Recorded::sys("open-bundle-ghostty"),
    ]);
    let (outcome, notes) = click::run(&state, None, &runner, &Spy::default(), &id, 2_000);

    assert!(
        matches!(outcome, Outcome::Focused { .. }),
        "outcome: {outcome:?}, notes: {notes:?}"
    );
    assert_eq!(
        notes,
        ["clients 40822=com.mitchellh.ghostty -> com.mitchellh.ghostty"]
    );
    assert_eq!(
        runner.calls.borrow().last().map(|c| c.join(" ")).as_deref(),
        Some("/usr/bin/open -b com.mitchellh.ghostty")
    );
    server.join().expect("fake socket");
}

/// No client attached any more (`pgrep-herdr-none`): the terminal from when
/// it was posted.
#[test]
fn with_no_client_left_the_click_raises_the_job_terminal() {
    let state = state_for("click_no_client");
    let (socket, server) = fake_herdr("ck_nocl", &SocketExchange::load("pane-focus-ok"));
    let id = job_id();
    let mut job = a_job(&id, 9_000, &socket);
    job.detect_at_click = true;
    state.save_job(&job).expect("save job");

    let none = Recorded::pgrep_herdr("pgrep-herdr-none");
    let runner = Replay::new([none, Recorded::sys("open-bundle-ghostty")]);
    let (outcome, notes) = click::run(&state, None, &runner, &Spy::default(), &id, 2_000);

    assert!(
        matches!(outcome, Outcome::Focused { .. }),
        "outcome: {outcome:?}, notes: {notes:?}"
    );
    assert_eq!(runner.call_count(), 2, "{:?}", runner.calls.borrow());
    assert_eq!(
        runner.calls.borrow()[1][2],
        "com.mitchellh.ghostty",
        "the job's bundle_id"
    );
    server.join().expect("fake socket");
}

/// With no terminal resolved there is nothing to raise, but the pane can
/// still be focused in whatever window Herdr is in.
#[test]
fn an_unresolved_terminal_still_focuses_the_pane() {
    let state = state_for("click_unresolved");
    let (socket, server) = fake_herdr("ck_unres", &SocketExchange::load("pane-focus-ok"));
    let id = job_id();
    let mut job = a_job(&id, 9_000, &socket);
    job.bundle_id = None;
    state.save_job(&job).expect("save job");

    let runner = Replay::default();
    let (outcome, notes) = click::run(&state, None, &runner, &Spy::default(), &id, 2_000);

    assert!(
        matches!(outcome, Outcome::Focused { .. }),
        "outcome: {outcome:?}, notes: {notes:?}"
    );
    assert_eq!(runner.call_count(), 0, "no bundle id, so no open -b");
    server.join().expect("fake socket");
}

#[test]
fn a_failed_open_still_focuses_the_pane() {
    let state = state_for("click_open_fails");
    let (socket, server) = fake_herdr("ck_openf", &SocketExchange::load("pane-focus-ok"));
    let id = job_id();
    let mut job = a_job(&id, 9_000, &socket);
    job.bundle_id = Some("io.github.dev.no-such-app".to_owned());
    state.save_job(&job).expect("save job");

    let runner = Replay::new([Recorded::sys("open-bundle-unknown")]);
    let (outcome, notes) = click::run(&state, None, &runner, &Spy::default(), &id, 2_000);

    assert!(
        matches!(outcome, Outcome::Focused { .. }),
        "outcome: {outcome:?}, notes: {notes:?}"
    );
    assert!(
        notes
            .iter()
            .any(|n| n.contains("LSCopyApplicationURLsForBundleIdentifier")),
        "open's stderr from sys/open-bundle-unknown should be logged: {notes:?}"
    );
    server.join().expect("fake socket");
}

/// The pane can close while its notification waits in Notification Center.
#[test]
fn a_pane_that_has_gone_is_not_focused_but_the_job_is_cleared() {
    let state = state_for("click_pane_gone");
    let (socket, server) = fake_herdr("ck_gone", &SocketExchange::load("pane-focus-not-found"));
    let id = job_id();
    state
        .save_job(&a_job(&id, 9_000, &socket))
        .expect("save job");

    let (outcome, notes) = click::run(&state, None, &opens_ghostty(), &Spy::default(), &id, 2_000);

    assert_eq!(
        outcome,
        Outcome::NotFocused {
            pane_id: "w3:p1".to_owned()
        }
    );
    assert!(
        notes.iter().any(|n| n.contains("pane_not_found")),
        "the error code from socket/pane-focus-not-found should be logged: {notes:?}"
    );
    assert!(
        matches!(state.job(&id), Ok(Loaded::Missing)),
        "the job file should be gone"
    );
    server.join().expect("fake socket");
}

/// Herdr not running is the same as the pane having gone, as far as the
/// user can tell.
#[test]
fn no_herdr_socket_is_not_focused() {
    let state = state_for("click_no_herdr");
    let id = job_id();
    state
        .save_job(&a_job(&id, 9_000, &no_socket("click_no_herdr")))
        .expect("save job");

    let (outcome, notes) = click::run(&state, None, &opens_ghostty(), &Spy::default(), &id, 2_000);

    assert!(
        matches!(outcome, Outcome::NotFocused { .. }),
        "outcome: {outcome:?}"
    );
    assert!(
        notes.iter().any(|n| n.starts_with("could not focus w3:p1")),
        "notes: {notes:?}"
    );
}

#[test]
fn clicking_a_live_job_clears_it() {
    let state = state_for("click_live");
    let id = job_id();
    let job = a_job(&id, 9_000, &no_socket("click_live"));
    state.save_job(&job).expect("save job");

    let spy = Spy::default();
    click::run(&state, None, &opens_ghostty(), &spy, &id, 2_000);

    assert_eq!(
        Spy::arg_after(&spy.only(), "-remove").as_deref(),
        Some("herdr-nudge-w3:p1"),
        "the group should be withdrawn"
    );
    assert_eq!(
        spy.only()[0],
        job.notifier_path.display().to_string(),
        "the notifier path should come from the job file"
    );
    assert!(
        matches!(state.job(&id), Ok(Loaded::Missing)),
        "the job file should be gone"
    );
}

#[test]
fn clicking_an_expired_job_clears_it_without_focusing() {
    let state = state_for("click_expired");
    let id = job_id();
    state
        .save_job(&a_job(&id, 1_500, &no_socket("click_expired")))
        .expect("save job");

    let runner = opens_ghostty();
    let (outcome, notes) = click::run(&state, None, &runner, &Spy::default(), &id, 1_500);

    assert_eq!(
        outcome,
        Outcome::Expired,
        "expiry is inclusive of the deadline"
    );
    assert_eq!(
        runner.call_count(),
        0,
        "an expired click should not raise the terminal"
    );
    assert!(
        !notes.iter().any(|n| n.contains("could not focus")),
        "nothing listens on the socket, so trying to focus would have been logged: {notes:?}"
    );
    assert!(
        matches!(state.job(&id), Ok(Loaded::Missing)),
        "an expired job should be tidied away"
    );
}

/// Another pane's notification must not be cleared by this one's click.
#[test]
fn a_click_leaves_another_panes_live_job_alone() {
    let state = state_for("click_other_pane");
    let id = job_id();
    state
        .save_job(&a_job(&id, 9_000, &no_socket("click_other_pane")))
        .expect("save job");

    let other_id = JobId::parse("fedcba9876543210").expect("16 hex");
    let mut other = a_job(&other_id, 9_000, &no_socket("click_other_pane"));
    other.pane_id = "w3:p2".to_owned();
    other.group = "herdr-nudge-w3:p2".to_owned();
    state.save_job(&other).expect("save other job");

    let spy = Spy::default();
    click::run(&state, None, &opens_ghostty(), &spy, &id, 2_000);

    assert!(
        matches!(state.job(&other_id), Ok(Loaded::Found(_))),
        "w3:p2's job should be untouched"
    );
    assert_eq!(
        Spy::arg_after(&spy.only(), "-remove").as_deref(),
        Some("herdr-nudge-w3:p1"),
        "only w3:p1's group should be withdrawn"
    );
}

/// The click has no HERDR_* environment, so it has to work out where the
/// state directory is.
#[test]
fn the_state_directory_is_found_without_any_herdr_environment() {
    let from_herdr = StateDir::locate(|name| match name {
        "HERDR_PLUGIN_STATE_DIR" => Some("/given/by/herdr".to_owned()),
        "HOME" => Some("/Users/dev".to_owned()),
        _ => None,
    });
    assert_eq!(
        from_herdr.map(|s| s.root),
        Some(PathBuf::from("/given/by/herdr")),
        "the variable wins when Herdr set it"
    );

    let from_home = StateDir::locate(|name| match name {
        "HOME" => Some("/Users/dev".to_owned()),
        _ => None,
    });
    assert_eq!(
        from_home.map(|s| s.root),
        Some(PathBuf::from(
            "/Users/dev/.local/state/herdr/plugins/herdr-nudge"
        )),
        "the path Herdr 0.9.0 uses, as captured in tests/fixtures/events/"
    );

    let nothing = StateDir::locate(|_| None);
    assert!(nothing.is_none(), "with no HOME there is nowhere to look");
}

/// An empty variable is not a path.
#[test]
fn an_empty_state_dir_variable_falls_through_to_home() {
    let located = StateDir::locate(|name| match name {
        "HERDR_PLUGIN_STATE_DIR" => Some(String::new()),
        "HOME" => Some("/Users/dev".to_owned()),
        _ => None,
    });
    assert_eq!(
        located.map(|s| s.root),
        Some(PathBuf::from(
            "/Users/dev/.local/state/herdr/plugins/herdr-nudge"
        )),
        "an empty HERDR_PLUGIN_STATE_DIR should not be used as a path"
    );
}

/// A failure to withdraw the notification must not leave the job behind, or
/// the next click would try again forever.
#[test]
fn the_job_is_deleted_even_if_the_notifier_will_not_start() {
    let state = state_for("click_notifier_fails");
    let (socket, server) = fake_herdr("ck_nfail", &SocketExchange::load("pane-focus-ok"));
    let id = job_id();
    state
        .save_job(&a_job(&id, 9_000, &socket))
        .expect("save job");

    let spy = Spy::failing();
    let (outcome, notes) = click::run(&state, None, &opens_ghostty(), &spy, &id, 2_000);

    assert!(
        matches!(outcome, Outcome::Focused { .. }),
        "the pane should still be focused: {outcome:?}"
    );
    assert!(
        notes.iter().any(|n| n.contains("could not remove group")),
        "the failure should be logged: {notes:?}"
    );
    assert!(
        matches!(state.job(&id), Ok(Loaded::Missing)),
        "the job file should still be deleted"
    );
    server.join().expect("fake socket");
}

/// A server started with `XDG_STATE_HOME` keeps its jobs under it, and the
/// click's environment doesn't have the variable.
#[test]
fn a_job_in_a_servers_own_state_directory_is_found() {
    let usual = state_for("click_elsewhere_usual");
    let theirs = StateDir::new(scratch_dir("click_elsewhere_xdg").join("state"));
    let id = job_id();
    theirs
        .save_job(&a_job(&id, 9_000, &no_socket("click_elsewhere")))
        .expect("save job");
    let server = ServerStateDir {
        pid: 15337,
        dir: theirs.clone(),
        protected: false,
    };

    let spy = Spy::default();
    let (outcome, notes) =
        click::run_with(&usual, |_| vec![server], &opens_ghostty(), &spy, &id, 2_000);

    assert!(
        matches!(outcome, Outcome::NotFocused { .. }),
        "the job should be found and acted on: {outcome:?} {notes:?}"
    );
    assert!(
        matches!(theirs.job(&id), Ok(Loaded::Missing)),
        "the job should be deleted where it was found"
    );
}

#[test]
fn a_state_directory_in_documents_is_not_read() {
    let usual = state_for("click_protected_usual");
    let theirs = StateDir::new(scratch_dir("click_protected_xdg").join("state"));
    let id = job_id();
    theirs
        .save_job(&a_job(&id, 9_000, &no_socket("click_protected")))
        .expect("save job");
    let server = ServerStateDir {
        pid: 15337,
        dir: theirs.clone(),
        protected: true,
    };

    let spy = Spy::default();
    let (outcome, notes) = click::run_with(
        &usual,
        |_| vec![server],
        &Replay::default(),
        &spy,
        &id,
        2_000,
    );

    assert_eq!(outcome, Outcome::NoJob);
    assert!(
        notes.iter().any(|n| n.contains("a folder macOS guards")),
        "notes: {notes:?}"
    );
    assert!(
        matches!(theirs.job(&id), Ok(Loaded::Found(_))),
        "the job should be left alone"
    );
}

/// The servers are only asked when the job isn't where it usually is.
#[test]
fn a_job_in_the_usual_place_does_not_ask_the_servers() {
    let state = state_for("click_usual_first");
    let id = job_id();
    state
        .save_job(&a_job(&id, 9_000, &no_socket("click_usual_first")))
        .expect("save job");

    let (outcome, _) = click::run_with(
        &state,
        |_| panic!("servers asked though the job was found"),
        &opens_ghostty(),
        &Spy::default(),
        &id,
        2_000,
    );
    assert!(matches!(outcome, Outcome::NotFocused { .. }), "{outcome:?}");
}
