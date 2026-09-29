//! Talking to Herdr. Queries go through the `herdr` CLI; focusing a pane
//! goes over the socket, because the CLI can't do it for a plain shell pane
//! (`herdr agent focus` only knows agent panes, and `herdr pane focus` only
//! moves by direction).
//!
//! Reply shapes are pinned by `tests/fixtures/cli/` and
//! `tests/fixtures/socket/`.

use std::collections::BTreeSet;
use std::fmt;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::process::Runner;

/// What `pane get` and `agent get` say about a pane. They return the same
/// shape under different keys (`pane` and `agent`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct PaneInfo {
    pub pane_id: String,
    pub workspace_id: String,
    #[serde(default)]
    pub tab_id: Option<String>,
    /// Follows the user's manual navigation, unlike the event context's
    /// `focused_pane_id`. Exactly one pane is focused across the server.
    pub focused: bool,
    #[serde(default)]
    pub agent: Option<String>,
    /// Set when something bound a session to the pane: an agent's own Herdr
    /// integration, or a reporter passing `report-agent
    /// --agent-session-id`. Detecting an agent from its screen never sets
    /// it, and no shell reporter we've captured binds one.
    #[serde(default)]
    pub agent_session: Option<serde_json::Value>,
    #[serde(default)]
    pub terminal_title_stripped: Option<String>,
}

impl PaneInfo {
    /// One half of the agent-or-shell decision. Herdr 0.9.0 leaves the key
    /// out rather than nulling it
    /// (`tests/fixtures/cli/pane-get-reported-no-session.json`); serde reads
    /// either as `None`.
    pub fn has_agent_session(&self) -> bool {
        self.agent_session.is_some()
    }
}

#[derive(Debug)]
pub enum Error {
    /// Couldn't run `herdr` or reach the socket, or it timed out.
    Io(io::Error),
    /// Herdr answered with an error object.
    Api { code: String, message: String },
    /// An answer we couldn't read.
    Unexpected(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::Api { code, message } => write!(f, "herdr: {code}: {message}"),
            Error::Unexpected(what) => write!(f, "unexpected reply from herdr: {what}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

/// Every reply, CLI or socket, is either `{"result": …}` or
/// `{"error": {"code", "message"}}`.
#[derive(Deserialize)]
struct Reply<T> {
    result: Option<T>,
    error: Option<ApiError>,
}

#[derive(Deserialize)]
struct ApiError {
    code: String,
    message: String,
}

fn parse_reply<T: DeserializeOwned>(text: &str) -> Result<T, Error> {
    let reply: Reply<T> =
        serde_json::from_str(text.trim()).map_err(|e| Error::Unexpected(format!("{e}: {text}")))?;
    match (reply.result, reply.error) {
        (_, Some(e)) => Err(Error::Api {
            code: e.code,
            message: e.message,
        }),
        (Some(result), None) => Ok(result),
        (None, None) => Err(Error::Unexpected(text.to_owned())),
    }
}

/// Agents Herdr has an integration for (`herdr integration status`) but no
/// detection manifest, so `server agent-manifests` leaves them out. Both
/// 0.9.0 and 0.9.1 ship these integrations, and each reports under this
/// label with a session. Treated as if the manifests named them, so the zsh
/// hook leaves their panes alone even when a pane query fails.
pub const AGENTS_WITHOUT_MANIFEST: &[&str] = &["omp", "mastracode"];

pub struct Cli<'a, R: Runner> {
    pub bin: &'a Path,
    pub runner: &'a R,
}

impl<R: Runner> Cli<'_, R> {
    /// Pane ids go in as plain arguments. `herdr` doesn't accept `--`, but
    /// it reads an unknown argument starting with `-` as a pane id rather
    /// than a flag (checked on 0.9.0), so a strange id just isn't found.
    /// `-h` and `--help` are still flags, and no id Herdr gives out looks
    /// like them.
    fn query<T: DeserializeOwned>(&self, args: &[&str]) -> Result<T, Error> {
        let out = self.runner.run(self.bin, args)?;
        // Errors come on stderr with exit 1; success on stdout with exit 0.
        if out.success() {
            parse_reply(&out.stdout)
        } else {
            match parse_reply::<serde_json::Value>(&out.stderr) {
                Err(api @ Error::Api { .. }) => Err(api),
                _ => Err(Error::Unexpected(format!(
                    "exit {:?}: {}",
                    out.code,
                    out.stderr.trim()
                ))),
            }
        }
    }

    pub fn pane_get(&self, pane_id: &str) -> Result<PaneInfo, Error> {
        #[derive(Deserialize)]
        struct Body {
            pane: PaneInfo,
        }
        self.query::<Body>(&["pane", "get", pane_id])
            .map(|r| r.pane)
    }

    /// Every pane the server has, across all workspaces. Only the ids are
    /// read.
    pub fn pane_ids(&self) -> Result<BTreeSet<String>, Error> {
        #[derive(Deserialize)]
        struct Pane {
            pane_id: String,
        }
        #[derive(Deserialize)]
        struct Body {
            panes: Vec<Pane>,
        }
        self.query::<Body>(&["pane", "list"])
            .map(|r| r.panes.into_iter().map(|p| p.pane_id).collect())
    }

    /// The agent labels Herdr can detect by itself, e.g. `claude`, `codex`.
    pub fn agent_manifests(&self) -> Result<Vec<String>, Error> {
        #[derive(Deserialize)]
        struct Body {
            manifests: Vec<Manifest>,
        }
        #[derive(Deserialize)]
        struct Manifest {
            agent: String,
        }
        self.query::<Body>(&["server", "agent-manifests", "--json"])
            .map(|r| r.manifests.into_iter().map(|m| m.agent).collect())
    }

    /// The name the user gave a workspace, which is what a banner shows.
    /// An event's context carries it, but a pane's environment has only the
    /// id. `None` for an empty label, which `workspace create --label ""`
    /// makes (`tests/fixtures/cli/workspace-get-empty-label.json`).
    pub fn workspace_label(&self, workspace_id: &str) -> Result<Option<String>, Error> {
        #[derive(Deserialize)]
        struct Body {
            workspace: Workspace,
        }
        #[derive(Deserialize)]
        struct Workspace {
            #[serde(default)]
            label: Option<String>,
        }
        self.query::<Body>(&["workspace", "get", workspace_id])
            .map(|r| r.workspace.label.filter(|label| !label.is_empty()))
    }

    /// Where Herdr keeps a plugin's config. A plain path on stdout, not JSON,
    /// and worked out without the server (0.9.0 and 0.9.1 both answer with
    /// none running).
    pub fn plugin_config_dir(&self, plugin_id: &str) -> Result<PathBuf, Error> {
        let out = self
            .runner
            .run(self.bin, &["plugin", "config-dir", plugin_id])?;
        let path = out.stdout.trim();
        if !out.success() || path.is_empty() {
            return Err(Error::Unexpected(format!(
                "exit {:?}: {}",
                out.code,
                out.stderr.trim()
            )));
        }
        Ok(PathBuf::from(path))
    }

    /// The running server's version, as `(0, 9, 2)`. Not the `herdr` on
    /// disk: after `herdr update` the old server runs on until a restart.
    /// The reply is a bare object, not `{"result": …}`
    /// (`tests/fixtures/cli/status-server.json`).
    pub fn server_version(&self) -> Result<(u32, u32, u32), Error> {
        #[derive(Deserialize)]
        struct Status {
            version: String,
        }
        let out = self.runner.run(self.bin, &["status", "server", "--json"])?;
        let text = out.stdout.trim();
        if !out.success() {
            return Err(Error::Unexpected(format!(
                "exit {:?}: {}",
                out.code,
                out.stderr.trim()
            )));
        }
        let status: Status =
            serde_json::from_str(text).map_err(|e| Error::Unexpected(format!("{e}: {text}")))?;
        parse_version(&status.version)
            .ok_or_else(|| Error::Unexpected(format!("version {:?}", status.version)))
    }
}

/// The first three numbers of `0.9.2`, `0.9.3-rc.1` or `1.0.0+build`.
pub fn parse_version(text: &str) -> Option<(u32, u32, u32)> {
    let mut parts = text.split(['.', '-', '+']).map(str::parse::<u32>);
    Some((
        parts.next()?.ok()?,
        parts.next()?.ok()?,
        parts.next()?.ok()?,
    ))
}

/// Asks Herdr to focus a pane, over the socket.
///
/// This is the call the click makes, and the click has no `HERDR_*`
/// environment, so the socket path has to come from the caller.
///
/// `timeout` bounds the reply, not the connect: `UnixStream` has no
/// connect timeout in std. A server that has stopped accepting would leave
/// a click waiting. Nothing is on screen by then, so the cost is a click
/// that appears to do nothing.
pub fn focus_pane(socket_path: &Path, pane_id: &str, timeout: Duration) -> Result<(), Error> {
    socket_request::<serde_json::Value>(
        socket_path,
        "pane.focus",
        serde_json::json!({ "pane_id": pane_id }),
        timeout,
    )
    .map(|_| ())
}

/// Whether Herdr passed a `notification.show` on to its clients, and why
/// not if it didn't: `rate_limited`, or with no client attached
/// `disabled` if toasts are off and `no_foreground_client` if they're on.
/// The binary also has `busy`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Shown {
    pub shown: bool,
    pub reason: String,
}

/// Asks Herdr to show a notification with its own `done` sound, over the
/// socket, so a title can't be taken for a CLI flag. Herdr plays the sound
/// and shows a toast by its own `[ui.sound]` and `[ui.toast]` settings, and
/// doesn't first check that a pane is still `done`, the check a finished
/// shell command fails from 0.9.2. It takes one of these a second across
/// the server, and does nothing with no client attached
/// (`tests/fixtures/socket/notification-show-0.9.2.json`, a server with no
/// client and toasts off, answers `disabled`).
pub fn show_notification(
    socket_path: &Path,
    title: &str,
    body: &str,
    timeout: Duration,
) -> Result<Shown, Error> {
    socket_request(
        socket_path,
        "notification.show",
        serde_json::json!({ "title": title, "body": body, "sound": "done" }),
        timeout,
    )
}

/// One request, one reply line.
fn socket_request<T: DeserializeOwned>(
    socket_path: &Path,
    method: &str,
    params: serde_json::Value,
    timeout: Duration,
) -> Result<T, Error> {
    let request = serde_json::json!({
        "id": "herdr-nudge",
        "method": method,
        "params": params,
    });

    let mut stream = UnixStream::connect(socket_path)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let mut line = request.to_string();
    line.push('\n');
    stream.write_all(line.as_bytes())?;

    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply)?;
    parse_reply(&reply)
}
