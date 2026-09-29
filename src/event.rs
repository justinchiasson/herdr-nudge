//! Types for the event JSON Herdr puts in `HERDR_PLUGIN_EVENT_JSON`.
//!
//! The shapes come from the captures in `tests/fixtures/events/`, not from
//! documentation. Worth knowing before changing them:
//!
//! - `pane.created` nests everything in a `pane` object, so there isn't
//!   always a `pane_id` at the top level.
//! - After an agent exits, the status event has no `agent` field at all.
//! - Shell reports leave `title`, `display_agent` and `state_labels` out
//!   rather than sending nulls.
//!
//! So besides the event's name, only `pane_id`, `workspace_id` and
//! `agent_status` are ever required (and `tab_id` on `tab.closed`), and an
//! event type or status we don't recognise parses as `Other` rather than
//! failing, so a Herdr update shouldn't break the plugin.
//!
//! What does still fail the parse: a payload with no `type` field, or one
//! missing a required field. Every captured event has them, and a failure
//! is logged and ignored rather than crashing the hook.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};

/// The whole event payload.
///
/// `event` here uses underscores (`pane_agent_status_changed`) while the
/// `HERDR_PLUGIN_EVENT` env var uses dots. We match on `data`'s own `type`
/// field instead; `event` is only used for logging.
#[derive(Debug, Clone, Deserialize)]
pub struct Envelope {
    pub event: String,
    pub data: EventData,
}

impl Envelope {
    pub fn parse(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventData {
    PaneAgentStatusChanged(StatusEvent),
    PaneAgentDetected(DetectedEvent),
    PaneFocused(PaneRef),
    PaneClosed(PaneRef),
    TabClosed(TabRef),
    WorkspaceClosed(WorkspaceRef),
    /// Events we don't handle: `pane.created`, `tab.created`, `tab.focused`,
    /// `workspace.focused`, and anything Herdr adds later.
    #[serde(other)]
    Other,
}

/// The only event that can lead to a notification.
#[derive(Debug, Clone, Deserialize)]
pub struct StatusEvent {
    pub pane_id: String,
    pub workspace_id: String,
    pub agent_status: AgentStatus,
    /// Missing when Herdr releases an agent it detected
    /// (`agent/status-unknown-no-agent-field.json`), and on a metadata
    /// report to a pane nobody claims. The release of a reported one still
    /// names it, whether the reporter sends it
    /// (`shell/status-unknown-on-release.json`) or Herdr 0.9.2 does
    /// (`shell/released-by-herdr-after-done-0.9.2.json`).
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub display_agent: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    /// e.g. `{"idle": "failed"}`. Herdr rewrites an unwatched `idle` to
    /// `done` but keeps these labels, so this is what tells us a command
    /// failed, not the status.
    #[serde(default, deserialize_with = "null_as_default")]
    pub state_labels: BTreeMap<String, String>,
}

/// An agent claiming a pane, or giving it up (`released`).
#[derive(Debug, Clone, Deserialize)]
pub struct DetectedEvent {
    pub pane_id: String,
    pub workspace_id: String,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub released: bool,
    #[serde(default)]
    pub final_status: Option<AgentStatus>,
}

/// `pane.focused` and `pane.closed` carry nothing else.
#[derive(Debug, Clone, Deserialize)]
pub struct PaneRef {
    pub pane_id: String,
    pub workspace_id: String,
}

/// `tab.closed`. It doesn't say which panes closed with the tab, and Herdr
/// sends no `pane.closed` for them (`tests/fixtures/events/lifecycle/tab-closed*`).
#[derive(Debug, Clone, Deserialize)]
pub struct TabRef {
    pub tab_id: String,
    pub workspace_id: String,
}

/// `workspace.closed`. It also carries a `workspace` object with counts, but
/// no pane or tab ids, and no `pane.closed` or `tab.closed` follows.
#[derive(Debug, Clone, Deserialize)]
pub struct WorkspaceRef {
    pub workspace_id: String,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Idle,
    Working,
    Blocked,
    Done,
    /// Herdr's own `"unknown"`, plus any status added later.
    #[serde(other)]
    Unknown,
}

impl AgentStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            AgentStatus::Idle => "idle",
            AgentStatus::Working => "working",
            AgentStatus::Blocked => "blocked",
            AgentStatus::Done => "done",
            AgentStatus::Unknown => "unknown",
        }
    }
}

impl fmt::Display for AgentStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl EventData {
    /// The pane this event is about, if it's about one.
    pub fn pane_id(&self) -> Option<&str> {
        match self {
            EventData::PaneAgentStatusChanged(e) => Some(&e.pane_id),
            EventData::PaneAgentDetected(e) => Some(&e.pane_id),
            EventData::PaneFocused(p) | EventData::PaneClosed(p) => Some(&p.pane_id),
            EventData::TabClosed(_) | EventData::WorkspaceClosed(_) | EventData::Other => None,
        }
    }

    pub fn workspace_id(&self) -> Option<&str> {
        match self {
            EventData::PaneAgentStatusChanged(e) => Some(&e.workspace_id),
            EventData::PaneAgentDetected(e) => Some(&e.workspace_id),
            EventData::PaneFocused(p) | EventData::PaneClosed(p) => Some(&p.workspace_id),
            EventData::TabClosed(t) => Some(&t.workspace_id),
            EventData::WorkspaceClosed(w) => Some(&w.workspace_id),
            EventData::Other => None,
        }
    }
}

/// Lets a field that isn't an `Option` accept an explicit null as well as a
/// missing key.
fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}
