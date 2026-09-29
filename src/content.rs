//! The three strings on a banner.
//!
//! Everything here comes from the event, the plugin context, or the pane
//! query, and every one of those fields is optional except the pane id. So
//! each line has a fallback chain that ends somewhere that always exists.

use crate::classify::PaneKind;
use crate::event::{AgentStatus, StatusEvent};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Content {
    pub title: String,
    pub subtitle: String,
    pub message: String,
}

/// `workspace_label` is the context's label for the workspace, and
/// `terminal_title` is `terminal_title_stripped` from the pane query. Both are
/// absent often enough to be worth passing as options rather than strings.
pub fn compose(
    kind: PaneKind,
    event: &StatusEvent,
    workspace_label: Option<&str>,
    terminal_title: Option<&str>,
) -> Content {
    Content {
        title: format!("{} · {}", subject(kind, event), status_word(event)),
        subtitle: workspace_label.unwrap_or(&event.workspace_id).to_owned(),
        message: event
            .title
            .as_deref()
            .or(terminal_title)
            .unwrap_or(&event.pane_id)
            .to_owned(),
    }
}

/// Who the notification is about: `Claude` for an agent, `make` for a shell
/// command.
///
/// An agent label is capitalised because it is a product name the user reads
/// as one. A shell label is the command they typed, so it stays as typed —
/// `Make test` would look like a different program.
fn subject(kind: PaneKind, event: &StatusEvent) -> String {
    let label = event
        .display_agent
        .as_deref()
        .or(event.agent.as_deref())
        .unwrap_or(&event.pane_id);

    match kind {
        // display_agent is the agent's own spelling, so leave it alone.
        PaneKind::Agent if event.display_agent.is_none() => capitalise(label),
        _ => label.to_owned(),
    }
}

/// The pane's own word for the state, else ours.
///
/// A reporter sets `state_labels` like `{"idle": "failed"}`. When the user
/// wasn't watching, Herdr rewrites the reported `idle` to `done` but leaves
/// the labels alone, so a `done` event has to look under `idle` to find
/// `failed` — see `tests/fixtures/events/shell/done-unwatched-failed.json`.
fn status_word(event: &StatusEvent) -> &str {
    if let Some(word) = event.state_labels.get(event.agent_status.as_str()) {
        return word;
    }
    if event.agent_status == AgentStatus::Done
        && let Some(word) = event.state_labels.get(AgentStatus::Idle.as_str())
    {
        return word;
    }
    event.agent_status.as_str()
}

fn capitalise(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
    }
}
