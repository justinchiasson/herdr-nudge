//! Files under `$HERDR_PLUGIN_STATE_DIR`.
//!
//! Every write goes to a new temp file in the same directory and is renamed
//! over the target, so a crash or a concurrent reader never sees half a
//! file. A file we can't parse is renamed aside and treated as missing: the
//! state here is all rebuildable, and a hook that fails on it every time
//! would stop notifications for good.
//!
//! Nothing is locked. The click process can run while an event hook does,
//! and Herdr doesn't wait for one hook before starting the next, so two
//! writers to the same file means the last one wins.

use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::classify::PaneKind;
use crate::cli::JobId;
use crate::event::AgentStatus;
use crate::process::Runner;
use crate::terminal;

/// Bumped when a file's shape changes. A file with any other version is
/// moved aside like a corrupt one.
pub const VERSION: u32 = 1;

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Writes `bytes` to `path` so that readers see the old file or the new
/// one, never a mix.
///
/// If `path` is a symlink, the rename replaces the link itself; the file it
/// pointed at is left alone.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty());
    let dir = dir.unwrap_or(Path::new("."));
    fs::create_dir_all(dir)?;

    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name"))?;
    let tmp = dir.join(format!(
        ".{}.{}.{}.tmp",
        name.to_string_lossy(),
        std::process::id(),
        next_tmp_counter()
    ));

    let result = (|| {
        // create_new fails if anything, a symlink included, is already there.
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&tmp, path)
    })();

    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn next_tmp_counter() -> u32 {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

pub fn write_json<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
    bytes.push(b'\n');
    write_atomic(path, &bytes)
}

#[derive(Debug)]
pub enum Loaded<T> {
    Missing,
    Found(T),
    /// The file couldn't be used and has been moved out of the way.
    Recovered(Recovered),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recovered {
    pub path: PathBuf,
    /// `None` if it couldn't be renamed and was deleted instead.
    pub moved_to: Option<PathBuf>,
    pub reason: String,
}

impl<T: Default> Loaded<T> {
    pub fn into_value(self) -> T {
        match self {
            Loaded::Found(value) => value,
            Loaded::Missing | Loaded::Recovered(_) => T::default(),
        }
    }
}

/// Reads a state file.
///
/// A symlink is moved aside, not followed. The click runs on behalf of our
/// notifier app, and a read that wandered into `~/Documents` would put up a
/// permission prompt under our name.
pub fn read_json<T: DeserializeOwned + Versioned>(path: &Path) -> io::Result<Loaded<T>> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Loaded::Missing),
        Err(e) => return Err(e),
    };
    if !meta.is_file() {
        return set_aside(path, "not a regular file".to_owned()).map(Loaded::Recovered);
    }

    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        // Another of our processes moved it aside between the two calls.
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Loaded::Missing),
        Err(e) => return Err(e),
    };
    let reason = match serde_json::from_slice::<T>(&bytes) {
        Ok(value) if value.version() == VERSION => return Ok(Loaded::Found(value)),
        Ok(value) => format!("version {}, expected {VERSION}", value.version()),
        Err(e) => e.to_string(),
    };
    set_aside(path, reason).map(Loaded::Recovered)
}

fn set_aside(path: &Path, reason: String) -> io::Result<Recovered> {
    let mut aside = path.as_os_str().to_owned();
    aside.push(format!(".corrupt-{}", now_ms()));
    let aside = PathBuf::from(aside);

    let moved_to = match fs::rename(path, &aside) {
        Ok(()) => Some(aside),
        Err(_) => match fs::remove_file(path) {
            Ok(()) => None,
            // Someone else already dealt with it, which is all we wanted.
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(e),
        },
    };
    Ok(Recovered {
        path: path.to_owned(),
        moved_to,
        reason,
    })
}

/// State files carry a version so a newer build's files aren't misread.
pub trait Versioned {
    fn version(&self) -> u32;
}

/// `agents-cache.json`: the agent labels Herdr detects by itself, from
/// `server agent-manifests`.
///
/// Cached because it costs a subprocess and changes about as often as Herdr
/// is upgraded. Refreshed when the server starts, and fetched by an event if
/// the file is missing, as it is when the plugin is installed into a Herdr
/// that is already running.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentsCache {
    pub version: u32,
    pub fetched_at_ms: u64,
    #[serde(default)]
    pub agents: BTreeSet<String>,
}

impl Default for AgentsCache {
    fn default() -> Self {
        AgentsCache {
            version: VERSION,
            fetched_at_ms: 0,
            agents: BTreeSet::new(),
        }
    }
}

impl Versioned for AgentsCache {
    fn version(&self) -> u32 {
        self.version
    }
}

impl AgentsCache {
    pub fn new(agents: impl IntoIterator<Item = String>, now_ms: u64) -> AgentsCache {
        AgentsCache {
            version: VERSION,
            fetched_at_ms: now_ms,
            agents: agents.into_iter().collect(),
        }
    }

    pub fn labels(&self) -> impl Iterator<Item = &str> {
        self.agents.iter().map(String::as_str)
    }
}

/// `jobs/<id>.json`: everything a click needs, written before the
/// notification is posted.
///
/// macOS launches the click command with no `HERDR_*` variables and a bare
/// `PATH`, so anything we learned from the environment has to be on disk by
/// then.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Job {
    pub version: u32,
    /// Also the file name.
    pub id: String,
    pub pane_id: String,
    pub workspace_id: String,
    pub agent_label: Option<String>,
    pub kind: PaneKind,
    /// Only ever a status the config asked for, so never `Unknown` — which
    /// matters because `Unknown` covers any status Herdr adds later and would
    /// be written back out as the word "unknown".
    pub status: AgentStatus,
    pub group: String,
    /// `None` means focus the pane without bringing an app forward. The
    /// terminal when this was posted; a click looks again, and uses this
    /// only if it finds none.
    pub bundle_id: Option<String>,
    /// Whether a click looks for the attached clients again, those of the
    /// server behind `socket_path`. False when `default_terminal` named the
    /// terminal, and for a job written before this field existed; the click
    /// then raises `bundle_id`.
    #[serde(default)]
    pub detect_at_click: bool,
    /// Posted for a label Herdr doesn't know as an agent, usually a shell
    /// command, on a Herdr server of 0.9.2 or later. A server whose version
    /// couldn't be read is assumed to be one. Such a server releases the
    /// label itself once the pane is back at a prompt, so a release doesn't
    /// mean the user moved on. False for a job written before this field
    /// existed.
    #[serde(default)]
    pub released_by_herdr: bool,
    pub socket_path: PathBuf,
    /// So the click can withdraw the notification.
    pub notifier_path: PathBuf,
    pub created_at_ms: u64,
    pub expires_at_ms: u64,
}

impl Versioned for Job {
    fn version(&self) -> u32 {
        self.version
    }
}

impl Job {
    /// Past this, the notification is no longer clickable and gets swept.
    pub fn is_expired(&self, now_ms: u64) -> bool {
        now_ms >= self.expires_at_ms
    }
}

/// A job id: 11 hex of the clock, 1 of a counter, 4 of the process id.
///
/// The clock and the pid separate us from other runs; the counter separates
/// two ids made by the same run in the same millisecond. One event posts at
/// most one notification, so the counter should never be needed — but an id
/// that collides silently overwrites another pane's live job, and a caller
/// that posts twice is not something the type stops. 44 bits of milliseconds
/// runs out in the year 2527.
///
/// Nothing here needs to be unguessable: a click only reads a file in our
/// own state directory.
pub fn new_job_id(now_ms: u64, pid: u32) -> JobId {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let id = format!(
        "{:011x}{:01x}{:04x}",
        now_ms & 0xfff_ffff_ffff,
        COUNTER.fetch_add(1, Ordering::Relaxed) & 0xf,
        pid & 0xffff
    );
    JobId::parse(&id).expect("a job id built from a clock, a counter and a pid is 16 lowercase hex")
}

/// The state directory and the files in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateDir {
    pub root: PathBuf,
}

pub const PLUGIN_ID: &str = "herdr-nudge";

impl StateDir {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        StateDir { root: root.into() }
    }

    /// `HERDR_PLUGIN_STATE_DIR` when we have it, the guessed path otherwise.
    ///
    /// A click has no `HERDR_*` environment and its argument is only a job
    /// id, so it has to find the state directory itself. Herdr 0.9.0 uses
    /// `~/.local/state/herdr/plugins/<plugin id>` (see the captured
    /// environment in any `tests/fixtures/events/` file). `XDG_STATE_HOME`
    /// is tried first, because Herdr 0.9.1 honours it: `herdr plugin
    /// config-dir` run with it set creates
    /// `$XDG_STATE_HOME/herdr/plugins/<plugin id>`, and so does a server
    /// started with it set. A click, run with a bare environment, won't have
    /// it even when the server does, so the click asks the servers
    /// ([`server_state_dirs`]) when the job isn't here.
    pub fn locate(lookup: impl Fn(&str) -> Option<String>) -> Option<StateDir> {
        if let Some(dir) = lookup("HERDR_PLUGIN_STATE_DIR").filter(|d| !d.is_empty()) {
            return Some(StateDir::new(dir));
        }
        let base = match lookup("XDG_STATE_HOME").filter(|d| !d.is_empty()) {
            Some(xdg) => PathBuf::from(xdg),
            None => PathBuf::from(lookup("HOME").filter(|h| !h.is_empty())?).join(".local/state"),
        };
        Some(StateDir::new(base.join("herdr/plugins").join(PLUGIN_ID)))
    }

    pub fn jobs_dir(&self) -> PathBuf {
        self.root.join("jobs")
    }

    pub fn job_path(&self, id: &JobId) -> PathBuf {
        self.jobs_dir().join(format!("{id}.json"))
    }

    pub fn job(&self, id: &JobId) -> io::Result<Loaded<Job>> {
        read_json(&self.job_path(id))
    }

    pub fn save_job(&self, job: &Job) -> io::Result<()> {
        let id = JobId::parse(&job.id)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
        write_json(&self.job_path(&id), job)
    }

    pub fn delete_job(&self, id: &JobId) -> io::Result<()> {
        match fs::remove_file(self.job_path(id)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Every job on disk. Names that aren't a job id are skipped, which is
    /// also how the `.tmp` and `.corrupt-*` files are left alone.
    pub fn job_ids(&self) -> io::Result<Vec<JobId>> {
        let entries = match fs::read_dir(self.jobs_dir()) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut ids = Vec::new();
        for entry in entries {
            let name = entry?.file_name();
            let Some(stem) = name.to_str().and_then(|n| n.strip_suffix(".json")) else {
                continue;
            };
            if let Ok(id) = JobId::parse(stem) {
                ids.push(id);
            }
        }
        ids.sort();
        Ok(ids)
    }

    pub fn shell_env_path(&self) -> PathBuf {
        self.root.join("shell.env")
    }

    /// The path users put in `~/.zshrc`, so it must never move.
    pub fn shell_hook_path(&self) -> PathBuf {
        self.root.join("herdr-nudge.zsh")
    }

    /// Which copy of the notifier was last registered with Launch Services.
    pub fn registered_path(&self) -> PathBuf {
        self.root.join("registered")
    }

    pub fn agents_cache_path(&self) -> PathBuf {
        self.root.join("agents-cache.json")
    }

    pub fn agents_cache(&self) -> io::Result<Loaded<AgentsCache>> {
        read_json(&self.agents_cache_path())
    }

    pub fn save_agents_cache(&self, cache: &AgentsCache) -> io::Result<()> {
        write_json(&self.agents_cache_path(), cache)
    }
}

/// The folders macOS asks permission for, under the user's home. A click
/// that read from one would put up that prompt under the Herdr Nudge name.
pub const PROTECTED_FOLDERS: [&str; 3] = ["Documents", "Downloads", "Desktop"];

/// Whether `path` is in one of [`PROTECTED_FOLDERS`] under `home`.
///
/// Compared without regard to case, as the Mac's disk usually is, and a
/// path with `..` in it counts as protected rather than being worked out.
/// A symlink into one of them isn't caught: seeing it means touching the
/// path, which is what this exists to avoid.
pub fn in_protected_folder(path: &Path, home: &Path) -> bool {
    use std::path::Component;
    if path.components().any(|c| c == Component::ParentDir) {
        return true;
    }
    let lower = |p: &Path| PathBuf::from(p.to_string_lossy().to_lowercase());
    let path = lower(path);
    PROTECTED_FOLDERS
        .iter()
        .any(|folder| path.starts_with(lower(&home.join(folder))))
}

/// Where a running Herdr server puts our state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerStateDir {
    pub pid: u32,
    pub dir: StateDir,
    /// In one of [`PROTECTED_FOLDERS`], so a click won't look there.
    pub protected: bool,
}

/// The state directory each running Herdr server gives its hooks, worked out
/// from the server's own environment the way [`StateDir::locate`] does.
///
/// For the click, which runs with a bare environment and so misses a
/// server's `XDG_STATE_HOME`. `ps` reads another process's environment
/// without any permission, as long as it's the same user's.
///
/// `home` is the caller's own `HOME`. A directory counts as protected under
/// it or under the server's, since a server can be started with neither
/// or with a different one.
pub fn server_state_dirs(
    runner: &impl Runner,
    home: Option<&Path>,
    notes: &mut Vec<String>,
) -> Vec<ServerStateDir> {
    let mut found = Vec::new();
    for (pid, args) in terminal::herdr_processes(runner, notes) {
        if !terminal::is_server(&args) {
            continue;
        }
        let Some(env) = terminal::process_env(runner, pid, args.len()) else {
            notes.push(format!("could not read herdr server {pid}'s environment"));
            continue;
        };
        let lookup = |name: &str| match name {
            "XDG_STATE_HOME" | "HOME" => env.get(name).cloned(),
            _ => None,
        };
        let Some(dir) = StateDir::locate(lookup) else {
            continue;
        };
        let server_home = env.get("HOME").map(Path::new);
        let protected = [home, server_home]
            .into_iter()
            .flatten()
            .any(|home| in_protected_folder(&dir.root, home));
        found.push(ServerStateDir {
            pid,
            dir,
            protected,
        });
    }
    found
}
