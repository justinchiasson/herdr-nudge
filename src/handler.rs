//! What we do about one event.
//!
//! The order matters and most of it is about not asking Herdr questions we
//! don't need answered. An event hook runs on every status change, including
//! the `working` churn of an agent that is just thinking, so the first thing
//! checked is whether any trigger set names this status at all. Past that
//! point one `herdr pane get` answers two questions at once: is the user
//! looking at this pane, and is it an agent or a shell command.
//!
//! Every event also reads `jobs/` once, before anything but the notifier's
//! registration check (`ensure_registered`). A job is a notification that
//! is up, and one that no longer says what the pane is doing gets taken
//! down: the pane changed status, closed (alone or with its tab or
//! workspace), or the user went to it. That read costs no subprocess,
//! and nothing is started unless a job turns out to be stale.
//!
//! Herdr doesn't wait for one hook before starting the next, so two can run
//! at once for the same pane, and nothing here locks. Two events for one
//! pane closer together than a posting hook takes (about 100 ms) can leave
//! a banner up whose job the other hook deleted, so clicking it does
//! nothing, or leave up one the second event should have taken down.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::classify::{self, Classification, ClassifySignal, PaneKind};
use crate::cli::JobId;
use crate::config::Config;
use crate::content;
use crate::context::Context;
use crate::event::{AgentStatus, Envelope, EventData, StatusEvent};
use crate::herdr::{self, Cli, PaneInfo};
use crate::notifier::{self, Notifier, Post};
use crate::process::{Runner, Spawner};
use crate::register;
use crate::state::{self, AgentsCache, Job, Loaded, StateDir, VERSION};
use crate::terminal;

pub struct Deps<'a, R: Runner, S: Spawner> {
    pub config: &'a Config,
    pub state: &'a StateDir,
    pub herdr_bin: &'a Path,
    pub socket_path: &'a Path,
    pub notifier_bin: &'a Path,
    /// Our own binary, for the click command. Must be absolute.
    pub self_bin: &'a Path,
    pub plugin_root: &'a Path,
    pub runner: &'a R,
    pub spawner: &'a S,
    pub now_ms: u64,
    pub pid: u32,
}

/// Why an event did or didn't become a notification. `main` logs this and the
/// tests assert on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// An event we don't subscribe to.
    NotHandled,
    /// No trigger set names this status, so nothing was asked of Herdr.
    StatusNotWatched(AgentStatus),
    Disabled(PaneKind),
    NotATrigger {
        kind: PaneKind,
        status: AgentStatus,
    },
    /// The shell command is in `ignore_commands`.
    IgnoredCommand(String),
    /// The agent is in `[agents] ignore`.
    IgnoredAgent(String),
    /// `notify_on_failure_only` is on and the command succeeded.
    NotAFailure,
    /// The user is looking at the pane in the frontmost terminal.
    Watching,
    /// A notification for this pane, agent and status is already showing.
    AlreadyShowing(String),
    NotifierMissing(PathBuf),
    /// Our own path can't be put in the click command, so no notification
    /// would be clickable.
    CannotBuildClick(String),
    Failed(String),
    Posted(Posted),
    /// The pane closed, or the user went to it, and its notification was
    /// taken down. Holds the group.
    Withdrawn(String),
    /// A tab or workspace closed and these panes went with it. Holds their
    /// groups.
    PanesGone(Vec<String>),
    /// The pane closed or got focus, or a tab or workspace closed, and
    /// nothing it had was up.
    NothingShowing,
    /// The pane got focus while no terminal showing Herdr was in front, so
    /// the user can't have seen it. A script moving focus looks like this.
    /// Its notification stays up.
    FocusedUnseen,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Posted {
    pub job_id: JobId,
    pub group: String,
    pub kind: PaneKind,
    /// `None` for `herdr-nudge test`, which says what kind it is.
    pub signal: Option<ClassifySignal>,
    pub title: String,
}

/// The outcome plus anything worth putting in `herdr plugin log`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub outcome: Outcome,
    pub notes: Vec<String>,
}

/// A job on disk and the id its file is named by.
pub type StoredJob = (JobId, Job);

/// Every job still clickable. Expired ones are withdrawn and deleted on the
/// way, so this also does the sweeping.
///
/// A job that can't be read is left out. A corrupt one has already been
/// renamed to `.corrupt-*` by `read_json`, and stays in `jobs/` as that.
pub fn live_jobs<S: Spawner>(
    state: &StateDir,
    spawner: &S,
    now_ms: u64,
    notes: &mut Vec<String>,
) -> Vec<StoredJob> {
    let ids = match state.job_ids() {
        Ok(ids) => ids,
        Err(e) => {
            notes.push(format!("could not list jobs: {e}"));
            return Vec::new();
        }
    };

    let mut live = Vec::new();
    let mut expired = Vec::new();
    for id in ids {
        let Ok(Loaded::Found(job)) = state.job(&id) else {
            continue;
        };
        if job.is_expired(now_ms) {
            expired.push((id, job));
        } else {
            live.push((id, job));
        }
    }
    if !expired.is_empty() {
        notes.push(format!("swept {} expired job(s)", expired.len()));
        // The group is per pane. An expired job left over next to a live one
        // for the same pane would take the live banner down with it.
        let (shadowed, alone): (Vec<_>, Vec<_>) = expired
            .iter()
            .partition(|(_, old)| live.iter().any(|(_, job)| job.pane_id == old.pane_id));
        forget(state, shadowed.into_iter(), notes);
        withdraw(state, spawner, alone.into_iter(), notes);
    }
    live
}

pub fn handle<R: Runner, S: Spawner>(
    deps: &Deps<R, S>,
    envelope: &Envelope,
    context: Option<&Context>,
) -> Report {
    let mut notes = Vec::new();
    ensure_registered(deps, &mut notes);
    let jobs = live_jobs(deps.state, deps.spawner, deps.now_ms, &mut notes);

    let outcome = match &envelope.data {
        EventData::PaneAgentStatusChanged(event) => {
            status_changed(deps, event, context, &jobs, &mut notes)
        }
        EventData::PaneClosed(pane) => {
            let mine = ours_for_pane(deps, &jobs, &pane.pane_id);
            withdraw_all(deps, &mine, &mut notes)
        }
        EventData::PaneFocused(pane) => focused(deps, &pane.pane_id, &jobs, &mut notes),
        EventData::TabClosed(_) => container_closed(deps, Closed::Tab, &jobs, &mut notes),
        EventData::WorkspaceClosed(workspace) => container_closed(
            deps,
            Closed::Workspace(&workspace.workspace_id),
            &jobs,
            &mut notes,
        ),
        _ => Outcome::NotHandled,
    };
    Report { outcome, notes }
}

/// First thing in a hook, before a withdrawal or a post can run the
/// notifier. After an update the copy on disk is new, and its first run
/// might be a `-remove`.
fn ensure_registered<R: Runner, S: Spawner>(deps: &Deps<R, S>, notes: &mut Vec<String>) {
    let bundle = notifier::bundle_path(deps.plugin_root);
    if let Some(result) = register::ensure(deps.state, deps.runner, &bundle, deps.now_ms) {
        notes.push(register::note(&bundle, result));
    }
}

/// What runs as Herdr's startup hook, once per server start (not when a
/// client attaches).
///
/// Every job from before `now_ms` goes. Herdr restores panes under the same
/// ids after a restart, so a job from before it could focus a different pane
/// than the one its banner is about. The cutoff keeps a job that a hook
/// running alongside this one posted after it started; one posted by a hook
/// that started before this one is still dropped. The agent
/// list is fetched again, since a restart is often a Herdr upgrade.
///
/// `herdr_bin` is `None` when the hook's environment didn't say where
/// `herdr` is. The list is then left as it was.
///
/// The notifier is registered first, whatever the marker says, before the
/// withdrawals run it. `bundle` is `None` when the plugin couldn't be found.
///
/// Returns the notes and the agent list if one was fetched, which the zsh
/// hook's install needs even when saving it failed.
pub fn cleanup<R: Runner, S: Spawner>(
    state: &StateDir,
    bundle: Option<&Path>,
    herdr_bin: Option<&Path>,
    runner: &R,
    spawner: &S,
    now_ms: u64,
) -> (Vec<String>, Option<AgentsCache>) {
    let mut notes = Vec::new();

    match bundle {
        Some(bundle) => notes.push(register::note(
            bundle,
            register::always(state, runner, bundle, now_ms),
        )),
        None => notes.push("no plugin folder, notifier not registered".to_owned()),
    }

    let mut jobs = Vec::new();
    match state.job_ids() {
        Ok(ids) => {
            for id in ids {
                // An unreadable job is skipped: without it there is no group
                // to withdraw. A corrupt one is renamed aside by `read_json`.
                if let Ok(Loaded::Found(job)) = state.job(&id)
                    && job.created_at_ms < now_ms
                {
                    jobs.push((id, job));
                }
            }
        }
        Err(e) => notes.push(format!("could not list jobs: {e}")),
    }
    if !jobs.is_empty() {
        notes.push(format!(
            "dropped {} job(s) from before the restart",
            jobs.len()
        ));
        withdraw(state, spawner, jobs.iter(), &mut notes);
    }

    let agents = match herdr_bin {
        Some(bin) => fetch_agents(&Cli { bin, runner }, state, now_ms, &mut notes),
        None => {
            notes.push("no HERDR_BIN_PATH, agent list not refreshed".to_owned());
            None
        }
    };
    (notes, agents)
}

/// `herdr-nudge test`: a banner for the pane it runs in, posted the way an
/// event's is, with a job file, so clicking it runs the real click.
///
/// The config's triggers and ignore lists aren't checked, and it isn't
/// suppressed as `Watching`: the user is at this pane when they run it, and
/// the point is to see a banner. What it says is made up, but laid out the
/// way a real agent or shell banner is.
///
/// It races hooks like any post does (see the module comment). With the zsh
/// hook holding a claim on the pane, typing the command releases it, and the
/// hook for that status change can take the new banner down if it runs late.
pub fn test<R: Runner, S: Spawner>(deps: &Deps<R, S>, pane_id: &str, kind: PaneKind) -> Report {
    let mut notes = Vec::new();
    ensure_registered(deps, &mut notes);
    let jobs = live_jobs(deps.state, deps.spawner, deps.now_ms, &mut notes);

    if !deps.notifier_bin.is_file() {
        let outcome = Outcome::NotifierMissing(deps.notifier_bin.to_owned());
        return Report { outcome, notes };
    }

    let cli = Cli {
        bin: deps.herdr_bin,
        runner: deps.runner,
    };
    // Also checks the pane is one this server has, since the id came from
    // the environment.
    let workspace_id = match cli.pane_get(pane_id) {
        Ok(info) => info.workspace_id,
        Err(e) => {
            let outcome = Outcome::Failed(format!("pane get {pane_id}: {e}"));
            return Report { outcome, notes };
        }
    };
    // Only the subtitle uses the label, so with it off there's nothing to ask.
    let label = if deps.config.notifications.show_workspace {
        match cli.workspace_label(&workspace_id) {
            Ok(label) => label,
            Err(e) => {
                notes.push(format!("workspace get {workspace_id}: {e}"));
                None
            }
        }
    } else {
        None
    };

    let event = test_event(pane_id, &workspace_id, kind);
    let resolution = terminal::resolve(deps.config, deps.runner, deps.socket_path, &mut notes);
    let content = content::compose(kind, &event, label.as_deref(), None);
    let subject = Subject {
        kind,
        agent_label: event.agent.as_deref(),
        // Nothing claimed the pane for a test, so nothing will release it.
        released_by_herdr: false,
    };
    let outcome = match post(deps, &event, &subject, &content, &resolution, &mut notes) {
        Ok((job_id, group)) => {
            // Replaced on screen by this one, same group.
            forget(deps.state, for_pane(&jobs, pane_id).into_iter(), &mut notes);
            Outcome::Posted(Posted {
                job_id,
                group,
                kind,
                signal: None,
                title: content.title,
            })
        }
        Err(outcome) => outcome,
    };
    Report { outcome, notes }
}

/// What a `blocked` agent or a finished shell command would send, give or
/// take the names.
fn test_event(pane_id: &str, workspace_id: &str, kind: PaneKind) -> StatusEvent {
    let (agent_status, display_agent, title, state_labels) = match kind {
        PaneKind::Agent => (
            AgentStatus::Blocked,
            Some("Herdr Nudge".to_owned()),
            "herdr-nudge test".to_owned(),
            Default::default(),
        ),
        // The zsh hook's own wording.
        PaneKind::Shell => (
            AgentStatus::Done,
            None,
            "herdr-nudge test --shell · exit 0 · 0s".to_owned(),
            [("idle".to_owned(), "done".to_owned())].into(),
        ),
    };
    StatusEvent {
        pane_id: pane_id.to_owned(),
        workspace_id: workspace_id.to_owned(),
        agent_status,
        agent: Some(state::PLUGIN_ID.to_owned()),
        display_agent,
        title: Some(title),
        state_labels,
    }
}

/// Takes the pane's notification down if the user went to the pane.
///
/// Herdr 0.9.1 sends `pane.focused` when the user moves to a pane by hand,
/// so this is what clears a banner once they have seen the pane
/// (`tests/fixtures/events/focus/manual-*`). 0.9.0 sends it only for focus
/// asked for through the CLI or the socket: our own click, which has deleted
/// the job by then, or some other tool's script. So on 0.9.0 this seldom
/// finds a job, and without one it costs nothing but the directory read.
///
/// Herdr's context says `invocation_source: "api"` for a mouse click too, so
/// the event can't tell a person from a script. The terminal in front can:
/// focus moved while the user was in another app is focus nobody saw.
fn focused<R: Runner, S: Spawner>(
    deps: &Deps<R, S>,
    pane_id: &str,
    jobs: &[StoredJob],
    notes: &mut Vec<String>,
) -> Outcome {
    let mine = ours_for_pane(deps, jobs, pane_id);
    if mine.is_empty() {
        return Outcome::NothingShowing;
    }
    let resolution = terminal::resolve(deps.config, deps.runner, deps.socket_path, notes);
    if !resolution.in_front(deps.runner) {
        return Outcome::FocusedUnseen;
    }
    withdraw_all(deps, &mine, notes)
}

/// A tab or workspace closed. Herdr sends no `pane.closed` for the panes
/// that went with it, and the event doesn't list them.
///
/// A workspace's panes are the jobs posted with its id: a pane id carries
/// its workspace, and a pane moved to another workspace gets a new id
/// there. So a closed workspace needs no question to Herdr.
///
/// A tab can't be matched that way. Jobs don't carry a tab id, and moving
/// a tab's last pane into another tab closes the tab while the pane lives
/// on (`tests/fixtures/events/lifecycle/tab-closed-by-pane-move.json`). So
/// a closed tab asks which panes are left, and takes down the notifications
/// of the ones that aren't. That also catches a pane moved to another
/// workspace, whose old id is gone. By the time the hook runs, `pane list`
/// no longer has the closed panes (checked live on 0.9.0 and 0.9.1). If the
/// list can't be had, the jobs stay until they expire or a click finds the
/// pane gone.
fn container_closed<R: Runner, S: Spawner>(
    deps: &Deps<R, S>,
    closed: Closed<'_>,
    jobs: &[StoredJob],
    notes: &mut Vec<String>,
) -> Outcome {
    let ours = ours(deps, jobs);
    if ours.is_empty() {
        return Outcome::NothingShowing;
    }
    let gone: Vec<&StoredJob> = match closed {
        Closed::Workspace(workspace_id) => ours
            .into_iter()
            .filter(|(_, job)| job.workspace_id == workspace_id)
            .collect(),
        Closed::Tab => {
            let cli = Cli {
                bin: deps.herdr_bin,
                runner: deps.runner,
            };
            match cli.pane_ids() {
                Ok(panes) => ours
                    .into_iter()
                    .filter(|(_, job)| !panes.contains(&job.pane_id))
                    .collect(),
                Err(e) => {
                    notes.push(format!("could not list panes: {e}"));
                    Vec::new()
                }
            }
        }
    };
    if gone.is_empty() {
        return Outcome::NothingShowing;
    }
    let groups: BTreeSet<String> = gone.iter().map(|(_, job)| job.group.clone()).collect();
    withdraw(deps.state, deps.spawner, gone.into_iter(), notes);
    Outcome::PanesGone(groups.into_iter().collect())
}

enum Closed<'a> {
    Tab,
    Workspace(&'a str),
}

fn withdraw_all<R: Runner, S: Spawner>(
    deps: &Deps<R, S>,
    mine: &[&StoredJob],
    notes: &mut Vec<String>,
) -> Outcome {
    // Every job for one pane has the same group.
    let Some((_, job)) = mine.first() else {
        return Outcome::NothingShowing;
    };
    let group = job.group.clone();
    withdraw(deps.state, deps.spawner, mine.iter().copied(), notes);
    Outcome::Withdrawn(group)
}

/// A status change: post it, or say why not, and take down whatever the
/// pane had up before if it no longer applies.
fn status_changed<R: Runner, S: Spawner>(
    deps: &Deps<R, S>,
    event: &StatusEvent,
    context: Option<&Context>,
    jobs: &[StoredJob],
    notes: &mut Vec<String>,
) -> Outcome {
    let mine = ours_for_pane(deps, jobs, &event.pane_id);

    // Only the newest is on screen, because the group is per pane and each
    // post replaced the one before. Anything older is left over from a
    // delete that failed and must not pass for what is showing.
    //
    // A repeat is normal: reporting new metadata alone emits a status event
    // carrying the unchanged status. Caught here, before any subprocess.
    //
    // An event with no label of its own matches on status alone, because the
    // job's label may then have come from `pane get` instead.
    let newest = mine.iter().max_by_key(|(_, job)| job.created_at_ms);
    if let Some((id, job)) = newest
        && job.status == event.agent_status
        && (event.agent.is_none() || job.agent_label.as_deref() == event.agent.as_deref())
    {
        let leftovers = mine.iter().copied().filter(|(other, _)| other != id);
        forget(deps.state, leftovers, notes);
        return Outcome::AlreadyShowing(id.to_string());
    }

    // Herdr 0.9.2 releases a shell command itself once the prompt is back,
    // within a second of the `done`
    // (`tests/fixtures/events/shell/released-by-herdr-after-done-0.9.2.json`).
    // The user hasn't come back, so the banner stays. When the next command
    // starts, the zsh hook clears the pane's title, and the `unknown` with
    // no agent that follows takes the banner down
    // (`shell/cleared-on-next-command-0.9.2.json`).
    //
    // On an older server the release comes from the reporter, when the next
    // command starts. It still takes the banner down there, because other
    // reporters, like herdr-ohmyzsh, don't send a clear after it.
    let released = event.agent_status == AgentStatus::Unknown
        && event.agent.is_some()
        && newest.is_some_and(|(_, job)| job.released_by_herdr && job.agent_label == event.agent);

    let outcome = notify(deps, event, context, notes);

    // Whatever was up is out of date: the pane has moved on, since it isn't
    // the status above. A new post already replaced it on screen, in the same
    // group, so only the files go. A `-remove` now could reach the notifier
    // after the post and take the new banner down with it.
    //
    // The group has no server in it, so the post also replaced another
    // session's banner for the same pane id. Its job goes too, or a later
    // withdrawal for it would take this banner down.
    if matches!(outcome, Outcome::Posted(_)) {
        forget(
            deps.state,
            for_pane(jobs, &event.pane_id).into_iter(),
            notes,
        );
    } else if released {
        notes.push("released, so the banner stays up".to_owned());
    } else if !mine.is_empty() {
        withdraw(deps.state, deps.spawner, mine.iter().copied(), notes);
    }
    outcome
}

/// Everything from the trigger gate to the banner.
fn notify<R: Runner, S: Spawner>(
    deps: &Deps<R, S>,
    event: &StatusEvent,
    context: Option<&Context>,
    notes: &mut Vec<String>,
) -> Outcome {
    // Before anything else, and without asking Herdr: could this status ever
    // produce a notification? The trigger set depends on whether the pane is
    // an agent, and that answer costs a subprocess, so the union of both sets
    // is what gets checked here.
    if !watched_by_any(deps.config, event.agent_status) {
        return Outcome::StatusNotWatched(event.agent_status);
    }

    let cli = Cli {
        bin: deps.herdr_bin,
        runner: deps.runner,
    };
    let info = match cli.pane_get(&event.pane_id) {
        Ok(info) => Some(info),
        Err(e) => {
            notes.push(format!("pane get {}: {e}", event.pane_id));
            None
        }
    };

    // The event's own label is the one this status is about. A pane that has
    // just been released has none, and then the query's is the best we have.
    let agent_label = event
        .agent
        .as_deref()
        .or(info.as_ref().and_then(|i| i.agent.as_deref()));

    let manifests = manifests(&cli, deps.state, deps.now_ms, notes);
    let classification = classify::classify(
        deps.config,
        &manifests,
        agent_label,
        info.as_ref().map(PaneInfo::has_agent_session),
    );

    if let Some(outcome) = decide(deps.config, classification, event, agent_label) {
        return outcome;
    }

    if !deps.notifier_bin.is_file() {
        return Outcome::NotifierMissing(deps.notifier_bin.to_owned());
    }

    let resolution = terminal::resolve(deps.config, deps.runner, deps.socket_path, notes);

    // What app is in front only matters when the user is on this pane, so the
    // two `lsappinfo` calls happen only then.
    let focused = info.as_ref().map(|i| i.focused);
    if focused == Some(true) && resolution.in_front(deps.runner) {
        return Outcome::Watching;
    }

    let content = content::compose(
        classification.kind,
        event,
        context.map(Context::workspace_display),
        info.as_ref()
            .and_then(|i| i.terminal_title_stripped.as_deref()),
    );
    let released_by_herdr = released_by_herdr(&cli, agent_label, &manifests, notes);
    let subject = Subject {
        kind: classification.kind,
        agent_label,
        released_by_herdr,
    };
    match post(deps, event, &subject, &content, &resolution, notes) {
        Ok((job_id, group)) => Outcome::Posted(Posted {
            job_id,
            group,
            kind: classification.kind,
            signal: Some(classification.signal),
            title: content.title,
        }),
        Err(outcome) => outcome,
    }
}

/// What a banner is about, worked out before it's posted.
struct Subject<'a> {
    kind: PaneKind,
    agent_label: Option<&'a str>,
    /// See [`released_by_herdr`].
    released_by_herdr: bool,
}

/// Writes the job and puts the banner up. Returns the job id and group, or
/// why nothing was posted.
fn post<R: Runner, S: Spawner>(
    deps: &Deps<R, S>,
    event: &StatusEvent,
    subject: &Subject<'_>,
    content: &content::Content,
    resolution: &terminal::Resolution,
    notes: &mut Vec<String>,
) -> Result<(JobId, String), Outcome> {
    let job_id = state::new_job_id(deps.now_ms, deps.pid);
    let execute = notifier::click_command(deps.self_bin, &job_id)
        .map_err(|e| Outcome::CannotBuildClick(e.to_string()))?;

    let group = notifier::group_for(&event.pane_id);
    let job = Job {
        version: VERSION,
        id: job_id.to_string(),
        pane_id: event.pane_id.clone(),
        workspace_id: event.workspace_id.clone(),
        agent_label: subject.agent_label.map(str::to_owned),
        kind: subject.kind,
        status: event.agent_status,
        group: group.clone(),
        bundle_id: resolution.bundle_id.clone(),
        // The config wins at click time too, so there is nothing to look
        // for again.
        detect_at_click: resolution.source != terminal::TerminalSource::DefaultConfig,
        released_by_herdr: subject.released_by_herdr,
        socket_path: deps.socket_path.to_owned(),
        notifier_path: deps.notifier_bin.to_owned(),
        created_at_ms: deps.now_ms,
        expires_at_ms: deps.now_ms.saturating_add(
            deps.config
                .notifications
                .clickable_secs
                .saturating_mul(1000),
        ),
    };

    // Before the job is written: it can run `defaults`, and a job waiting
    // on disk with no banner yet is one a concurrent hook can delete first.
    let image = logo_for(deps, subject.kind, subject.agent_label);

    // Written before anything is on screen, because a banner can be clicked
    // the moment it appears and a click with no job file does nothing.
    if let Err(e) = deps.state.save_job(&job) {
        return Err(Outcome::Failed(format!("could not write job: {e}")));
    }

    let notifier = Notifier {
        binary: deps.notifier_bin,
        spawner: deps.spawner,
    };
    let post = Post {
        title: &content.title,
        subtitle: deps
            .config
            .notifications
            .show_workspace
            .then_some(content.subtitle.as_str()),
        message: &content.message,
        group: &group,
        content_image: image.as_deref(),
        sound: deps.config.notifications.sound,
        execute: &execute,
    };
    if let Err(e) = notifier.post(&post) {
        // Nothing reached the screen, so the job must not stay: it would
        // suppress the next event as a duplicate of a banner that never
        // existed.
        delete_job(deps.state, &job_id, notes);
        return Err(Outcome::Failed(format!(
            "could not start the notifier: {e}"
        )));
    }
    // Only a shell command's release is certain: its prompt is back as it
    // finishes. An agent that stays running after its `done` isn't
    // released, and Herdr plays its own sound. Herdr plays nothing for an
    // `idle`, so neither do we.
    if subject.released_by_herdr
        && subject.kind == PaneKind::Shell
        && event.agent_status == AgentStatus::Done
    {
        show_in_herdr(deps, content, notes);
    }
    Ok((job_id, group))
}

/// Whether Herdr releases this pane itself once the prompt is back. From
/// 0.9.2 it does that for a label a reporter claimed and Herdr doesn't
/// recognise as an agent, whatever the status and whatever we classify it
/// as. For our hook's commands the release came 93 to 532 ms after the
/// `done` in twelve live runs
/// (`shell/released-by-herdr-after-done-0.9.2.json`). Two things follow.
/// The release doesn't mean the user moved on. And Herdr shows its toast
/// and plays its `done` sound only if the pane is still `done` about a
/// second later (`[ui.toast] delay_seconds`), so for a shell command it
/// usually does neither, and we ask it to. If the release comes later than
/// that, or `delay_seconds` is 0, the user gets both. A custom agent run
/// once, which exits as it finishes, loses Herdr's sound too; we don't ask
/// for that one. Up to 0.9.1 the pane stays as it is until the next
/// command.
///
/// An agent put down as a shell command by `known_agents_remove` is still
/// one Herdr recognises, so it's never released. Before the agent list is
/// first cached, every label looks unrecognised.
fn released_by_herdr<R: Runner>(
    cli: &Cli<'_, R>,
    agent_label: Option<&str>,
    manifests: &BTreeSet<String>,
    notes: &mut Vec<String>,
) -> bool {
    let Some(label) = agent_label else {
        return false;
    };
    if manifests.contains(label) || herdr::AGENTS_WITHOUT_MANIFEST.contains(&label) {
        return false;
    }
    match cli.server_version() {
        Ok(version) => version >= (0, 9, 2),
        // Taken as 0.9.2 or later: on an older server that means two sounds
        // and a banner that stays until the next command, where the other
        // way a newer server's banner would come down at once, silent.
        Err(e) => {
            notes.push(format!("could not ask the server's version: {e}"));
            true
        }
    }
}

/// Has Herdr show the banner's text with its `done` sound, which it does by
/// the user's Herdr settings. A second one within a second is dropped by
/// Herdr, so two commands ending together get one sound.
fn show_in_herdr<R: Runner, S: Spawner>(
    deps: &Deps<R, S>,
    content: &content::Content,
    notes: &mut Vec<String>,
) {
    match herdr::show_notification(
        deps.socket_path,
        &content.title,
        &content.message,
        Duration::from_secs(1),
    ) {
        Ok(reply) if reply.shown => {}
        Ok(reply) => notes.push(format!("herdr showed nothing: {}", reply.reason)),
        Err(e) => notes.push(format!("could not ask herdr for its sound: {e}")),
    }
}

/// Either side of the config could want this status. Both are checked with
/// their `enabled` flag, so a disabled half doesn't buy a subprocess.
fn watched_by_any(config: &Config, status: AgentStatus) -> bool {
    (config.agents.enabled && config.agents.statuses.contains(&status))
        || (config.shell.enabled && config.shell.statuses.contains(&status))
}

/// The checks that only need the config and the event. `None` means carry on.
fn decide(
    config: &Config,
    classification: Classification,
    event: &StatusEvent,
    agent_label: Option<&str>,
) -> Option<Outcome> {
    let (enabled, statuses) = match classification.kind {
        PaneKind::Agent => (config.agents.enabled, &config.agents.statuses),
        PaneKind::Shell => (config.shell.enabled, &config.shell.statuses),
    };
    if !enabled {
        return Some(Outcome::Disabled(classification.kind));
    }
    if !statuses.contains(&event.agent_status) {
        return Some(Outcome::NotATrigger {
            kind: classification.kind,
            status: event.agent_status,
        });
    }

    if classification.kind == PaneKind::Agent
        && let Some(label) = agent_label
        && config.agents.ignore.iter().any(|a| a == label)
    {
        return Some(Outcome::IgnoredAgent(label.to_owned()));
    }

    // Checked here rather than left to our zsh hook, because any shell hook
    // can report one of these. A shell reporter's label is its command name.
    if classification.kind == PaneKind::Shell
        && let Some(label) = agent_label
        && config.shell.ignore_commands.iter().any(|c| c == label)
    {
        return Some(Outcome::IgnoredCommand(label.to_owned()));
    }

    // The label survives Herdr rewriting `idle` to `done`, so `idle` is the
    // key to look under for both statuses.
    if classification.kind == PaneKind::Shell
        && config.shell.notify_on_failure_only
        && event.state_labels.get("idle").map(String::as_str) != Some("failed")
    {
        return Some(Outcome::NotAFailure);
    }

    None
}

/// Herdr's agent list, from the cache. Fetched here when the cache is
/// missing, which it is when the plugin was installed into a running Herdr:
/// the startup hook that normally writes it hasn't run yet. Without it every
/// agent without its own Herdr integration reads as a shell command.
fn manifests<R: Runner>(
    cli: &Cli<R>,
    state: &StateDir,
    now_ms: u64,
    notes: &mut Vec<String>,
) -> BTreeSet<String> {
    match state.agents_cache() {
        Ok(Loaded::Found(cache)) => return cache.agents,
        Ok(Loaded::Missing) => {}
        Ok(Loaded::Recovered(r)) => notes.push(format!("agents cache set aside: {}", r.reason)),
        Err(e) => notes.push(format!("agents cache: {e}")),
    }
    fetch_agents(cli, state, now_ms, notes)
        .map(|cache| cache.agents)
        .unwrap_or_default()
}

/// Asks Herdr for its agent list and caches it. A list that can't be saved
/// is still returned, so the event that fetched it can use it.
fn fetch_agents<R: Runner>(
    cli: &Cli<R>,
    state: &StateDir,
    now_ms: u64,
    notes: &mut Vec<String>,
) -> Option<AgentsCache> {
    let agents = match cli.agent_manifests() {
        Ok(agents) => agents,
        Err(e) => {
            notes.push(format!("could not fetch the agent list: {e}"));
            return None;
        }
    };
    let cache = AgentsCache::new(agents, now_ms);
    match state.save_agents_cache(&cache) {
        Ok(()) => notes.push(format!("fetched the agent list ({})", cache.agents.len())),
        Err(e) => notes.push(format!("could not save the agent list: {e}")),
    }
    Some(cache)
}

/// Every job for this pane id, whichever server posted it: what a new post
/// replaces on screen, since the group is the pane id alone.
fn for_pane<'a>(jobs: &'a [StoredJob], pane_id: &str) -> Vec<&'a StoredJob> {
    jobs.iter()
        .filter(|(_, job)| job.pane_id == pane_id)
        .collect()
}

/// The jobs this server posted. A second Herdr session numbers its panes
/// the same way, so its `w1:p1` closing or getting focus says nothing
/// about ours.
fn ours<'a, R: Runner, S: Spawner>(deps: &Deps<R, S>, jobs: &'a [StoredJob]) -> Vec<&'a StoredJob> {
    jobs.iter()
        .filter(|(_, job)| job.socket_path == deps.socket_path)
        .collect()
}

fn ours_for_pane<'a, R: Runner, S: Spawner>(
    deps: &Deps<R, S>,
    jobs: &'a [StoredJob],
    pane_id: &str,
) -> Vec<&'a StoredJob> {
    ours(deps, jobs)
        .into_iter()
        .filter(|(_, job)| job.pane_id == pane_id)
        .collect()
}

/// Takes the notifications down and deletes the jobs. The group is per pane,
/// so jobs for one pane share a single `-remove`.
fn withdraw<'a, S: Spawner>(
    state: &StateDir,
    spawner: &S,
    jobs: impl Iterator<Item = &'a StoredJob>,
    notes: &mut Vec<String>,
) {
    let mut removed = BTreeSet::new();
    for (id, job) in jobs {
        if removed.insert((&job.notifier_path, &job.group)) {
            let notifier = Notifier {
                binary: &job.notifier_path,
                spawner,
            };
            match notifier.remove(&job.group) {
                Ok(()) => notes.push(format!("withdrew {} ({})", job.group, job.status)),
                Err(e) => notes.push(format!("could not remove group {}: {e}", job.group)),
            }
        }
        delete_job(state, id, notes);
    }
}

/// Deletes the jobs and leaves the screen alone.
fn forget<'a>(
    state: &StateDir,
    jobs: impl Iterator<Item = &'a StoredJob>,
    notes: &mut Vec<String>,
) {
    for (id, _) in jobs {
        delete_job(state, id, notes);
    }
}

fn delete_job(state: &StateDir, id: &JobId, notes: &mut Vec<String>) {
    if let Err(e) = state.delete_job(id) {
        notes.push(format!("could not delete job {id}: {e}"));
    }
}

/// Where the agent logos are, from the plugin root. `tools/fetch-icons.sh`
/// writes them there.
pub const LOGO_DIR: &str = "assets/agents";

/// Whether a label can name a file in [`LOGO_DIR`].
///
/// The label reaches us from Herdr or from a shell hook and becomes part of
/// a path, so a `/` or a `..` in one must not point somewhere else.
pub fn is_logo_label(label: &str) -> bool {
    !label.is_empty()
        && !label.starts_with('.')
        && label
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_'))
}

/// The agent's logo for the right of the banner, if we have one.
///
/// A missing file is never an error — the banner then shows the Herdr icon
/// alone. Shell commands get no logo.
fn logo_for<R: Runner, S: Spawner>(
    deps: &Deps<R, S>,
    kind: PaneKind,
    agent_label: Option<&str>,
) -> Option<PathBuf> {
    if !deps.config.notifications.agent_logos || kind != PaneKind::Agent {
        return None;
    }
    let label = agent_label?;
    if !is_logo_label(label) {
        return None;
    }
    let icons = deps.plugin_root.join(LOGO_DIR);
    let file = format!("{label}.png");
    let light = icons.join(&file);
    if !light.is_file() {
        return None;
    }
    // The banner's background shows through a logo's transparent parts, so a
    // black logo vanishes in dark mode and a white one in light mode. Logos
    // with that problem ship a second file for dark mode, and only those
    // cost the appearance query.
    let dark = icons.join("dark").join(&file);
    if dark.is_file() && notifier::dark_mode(deps.runner) {
        return Some(dark);
    }
    Some(light)
}
