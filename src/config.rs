//! `config.toml` in `$HERDR_PLUGIN_CONFIG_DIR`, and the `shell.env` file the
//! zsh hook reads.
//!
//! A missing file means all defaults. Unknown keys are an error rather than
//! ignored, so a typo like `defualt_terminal` shows up instead of silently
//! doing nothing.

use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize};

use crate::event::AgentStatus;
use crate::herdr::{self, Cli};
use crate::process::Runner;
use crate::state::PLUGIN_ID;

pub const FILE_NAME: &str = "config.toml";

/// `Serialize` is there so a test can check that [`example`] names every
/// key; nothing writes a config this way.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub default_terminal: Option<String>,
    pub notifications: Notifications,
    pub agents: Agents,
    pub shell: Shell,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Notifications {
    /// How long a notification can still be clicked from Notification
    /// Center. How long a banner stays on screen is the user's macOS alert
    /// style (Temporary or Persistent), not something we control.
    pub clickable_secs: u64,
    /// Off by default because Herdr usually plays its own sound for the
    /// same `blocked` or `done`, a moment after the banner, so both on means
    /// two sounds. With this off a banner is silent when Herdr plays nothing:
    /// for `idle`, while no Herdr client is attached, when its `[ui.sound]`
    /// is off or mutes that agent (droid by default), and for a `done` the
    /// pane has already left by the time Herdr checks. From Herdr 0.9.2 that
    /// last one is usually the case for a shell command's `done`, so the
    /// handler asks Herdr to play its sound then, whatever this says.
    pub sound: bool,
    pub agent_logos: bool,
    pub show_workspace: bool,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Agents {
    pub enabled: bool,
    #[serde(deserialize_with = "statuses")]
    pub statuses: Vec<AgentStatus>,
    /// Agents to stay quiet about, by the label Herdr reports (`claude`,
    /// `codex`), not the display name. Only panes classified as agents; a
    /// shell command is muted with `[shell] ignore_commands`.
    pub ignore: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Shell {
    pub enabled: bool,
    pub min_seconds: u64,
    /// `done` is in the default because Herdr turns a reported `idle` into
    /// `done` when the user wasn't watching the pane, and that's the case
    /// worth a notification. See `tests/fixtures/events/shell/done-unwatched-failed.json`.
    #[serde(deserialize_with = "statuses")]
    pub statuses: Vec<AgentStatus>,
    pub notify_on_failure_only: bool,
    /// Matched against the label a shell hook reports, which is its command
    /// name, and only for panes classified as shell commands. Also written
    /// into `shell.env` for our zsh hook.
    pub ignore_commands: Vec<String>,
    pub known_agents_extra: Vec<String>,
    pub known_agents_remove: Vec<String>,
}

impl Default for Notifications {
    fn default() -> Self {
        Notifications {
            clickable_secs: 3600,
            sound: false,
            agent_logos: true,
            show_workspace: true,
        }
    }
}

impl Default for Agents {
    fn default() -> Self {
        Agents {
            enabled: true,
            statuses: vec![AgentStatus::Blocked, AgentStatus::Done],
            ignore: Vec::new(),
        }
    }
}

impl Default for Shell {
    fn default() -> Self {
        Shell {
            enabled: true,
            min_seconds: 5,
            statuses: vec![AgentStatus::Idle, AgentStatus::Done],
            notify_on_failure_only: false,
            // Programs that only end when the user quits them, so "finished"
            // says nothing, and a claim would list the pane as working in
            // Herdr the whole time they're open. No REPLs: `python`
            // can't be told apart from `python train.py`, and a REPL is quit
            // at its own pane, where the banner is suppressed anyway.
            ignore_commands: strings(&[
                "vim", "vi", "nvim", "emacs", "nano", "less", "more", "man", "ssh", "mosh", "tmux",
                "screen", "zellij", "top", "htop", "btop", "lazygit", "tig", "fzf",
            ]),
            known_agents_extra: Vec::new(),
            known_agents_remove: Vec::new(),
        }
    }
}

fn strings(words: &[&str]) -> Vec<String> {
    words.iter().map(|s| s.to_string()).collect()
}

/// Only real statuses. `AgentStatus` itself turns any unknown word into
/// `Unknown` so a new Herdr status can't break event parsing, but in the
/// config an unknown word is a typo, and should say so.
fn statuses<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<AgentStatus>, D::Error> {
    let words = Vec::<String>::deserialize(deserializer)?;
    words
        .iter()
        .map(|word| match word.as_str() {
            "idle" => Ok(AgentStatus::Idle),
            "working" => Ok(AgentStatus::Working),
            "blocked" => Ok(AgentStatus::Blocked),
            "done" => Ok(AgentStatus::Done),
            other => Err(serde::de::Error::custom(format!(
                "unknown status {other:?}, expected idle, working, blocked or done"
            ))),
        })
        .collect()
}

#[derive(Debug)]
pub struct ConfigError {
    pub path: PathBuf,
    pub message: String,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path.display(), self.message)
    }
}

impl std::error::Error for ConfigError {}

impl Config {
    pub fn path(config_dir: &Path) -> PathBuf {
        config_dir.join(FILE_NAME)
    }

    /// Defaults if there's no file.
    pub fn load(config_dir: &Path) -> Result<Config, ConfigError> {
        let path = Config::path(config_dir);
        let error = |message: String| ConfigError {
            path: path.clone(),
            message,
        };
        match fs::read_to_string(&path) {
            Ok(text) => Config::parse(&text).map_err(error),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(error(e.to_string())),
        }
    }

    pub fn parse(text: &str) -> Result<Config, String> {
        let config: Config = toml::from_str(text).map_err(|e| e.to_string())?;
        config.check()?;
        Ok(config)
    }

    /// An empty bundle id would reach `open -b ""` on a click.
    fn check(&self) -> Result<(), String> {
        if self.default_terminal.as_deref().is_some_and(str::is_empty) {
            return Err("default_terminal is empty".to_owned());
        }
        Ok(())
    }

    /// Herdr's agent catalogue, plus the agents it integrates without a
    /// manifest, with our config applied on top: `extra` added, then
    /// `remove` taken out. So a label in both lists ends up removed.
    pub fn agent_catalogue<'a>(
        &self,
        fetched: impl IntoIterator<Item = &'a str>,
    ) -> BTreeSet<String> {
        let mut catalogue: BTreeSet<String> = fetched
            .into_iter()
            .chain(herdr::AGENTS_WITHOUT_MANIFEST.iter().copied())
            .map(str::to_owned)
            .collect();
        catalogue.extend(self.shell.known_agents_extra.iter().cloned());
        for label in &self.shell.known_agents_remove {
            catalogue.remove(label);
        }
        catalogue
    }
}

/// The settings the zsh hook needs, as `key=value` lines.
///
/// Not shell syntax: the hook has to split each line on the first `=` and
/// must never source or eval the file, or a value could run code. Command
/// names are compared word by word, so an entry with whitespace or a
/// control character could never match and would only break the line
/// format. Those are left out and returned so the caller can log them.
pub fn shell_env(config: &Config, catalogue: &BTreeSet<String>) -> (String, Vec<String>) {
    let mut out =
        String::from("# Written by herdr-nudge from config.toml. Edits are overwritten.\n");
    out.push_str(&format!("enabled={}\n", u8::from(config.shell.enabled)));
    // zsh integers are signed, so a u64 past i64::MAX would wrap negative
    // there and time every command. A year is longer than anything runs.
    const MAX_MIN_SECONDS: u64 = 365 * 24 * 3600;
    out.push_str(&format!(
        "min_seconds={}\n",
        config.shell.min_seconds.min(MAX_MIN_SECONDS)
    ));

    let mut skipped = Vec::new();
    let mut list = |key: &str, values: &mut dyn Iterator<Item = &String>| {
        for value in values {
            if value.is_empty() || value.chars().any(|c| c.is_whitespace() || c.is_control()) {
                skipped.push(value.clone());
            } else {
                out.push_str(&format!("{key}={value}\n"));
            }
        }
    };
    list("ignore", &mut config.shell.ignore_commands.iter());
    list("agent", &mut catalogue.iter());

    (out, skipped)
}

/// Where `config.toml` lives. A hook has `HERDR_PLUGIN_CONFIG_DIR`; from a
/// shell, `herdr` says, without needing the server. `from_env` is the
/// variable if it's set and not empty.
pub fn locate_dir<R: Runner>(
    from_env: Option<PathBuf>,
    herdr_bin: &Path,
    runner: &R,
) -> Result<PathBuf, herdr::Error> {
    match from_env {
        Some(dir) => Ok(dir),
        None => Cli {
            bin: herdr_bin,
            runner,
        }
        .plugin_config_dir(PLUGIN_ID),
    }
}

/// Writes [`example`] to `config.toml` in `config_dir`, creating the
/// directory if it has to. Returns false, having written nothing, if there
/// is already a file there, or a symlink, even one pointing nowhere.
///
/// Written to a temp file and linked into place, so a hook never reads half
/// a file and a failed write leaves nothing behind. A link, unlike a rename,
/// fails if the name is taken, so nothing is ever replaced.
pub fn write_example(config_dir: &Path) -> io::Result<bool> {
    fs::create_dir_all(config_dir)?;
    let path = Config::path(config_dir);
    let temp = config_dir.join(format!(".{FILE_NAME}.{}.tmp", std::process::id()));
    let written = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .and_then(|mut file| {
            io::Write::write_all(&mut file, example().as_bytes())?;
            file.sync_all()
        })
        .and_then(|()| fs::hard_link(&temp, &path));
    // Whether the link worked or not, the temp name has to go.
    let _ = fs::remove_file(&temp);
    match written {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(e),
    }
}

/// A `config.toml` with every key at its default and a line or two on each.
///
/// The values come from `Config::default()`, so they can't drift from it.
/// `default_terminal` stays commented out: set, it beats the terminal found
/// from the attached client, and would pin one terminal for good.
pub fn example() -> String {
    let d = Config::default();
    let n = &d.notifications;
    let a = &d.agents;
    let sh = &d.shell;
    format!(
        r#"# herdr-nudge settings. Every key is shown at its default, so delete
# any you don't change. A list you set replaces the default list whole.
# A key this version doesn't know is an error, and then the whole file is
# ignored until it's fixed (the plugin log says so).
#
# Most changes apply from the next notification. The zsh hook's settings
# ([shell] enabled, min_seconds, ignore_commands and the known_agents lists)
# reach it only after a Herdr restart, in shells started after that.

# The app a click brings forward, by bundle id. Left unset, it's the
# terminal of the Herdr client you used last, and a switch to another
# terminal is followed. Set it only if that guess is wrong: it then always
# wins.
# default_terminal = "com.mitchellh.ghostty"

[notifications]
# How long a notification can still be clicked in Notification Center.
# How long a banner stays on screen is macOS's alert style for Herdr Nudge:
# Temporary or Persistent (Banners or Alerts on older macOS).
clickable_secs = {clickable_secs}
# Herdr usually plays its own sound for the same event; true also plays the
# macOS notification sound with each notification.
sound = {sound}
# The agent's logo on the right of the banner.
agent_logos = {agent_logos}
# The workspace's name under the title.
show_workspace = {show_workspace}

[agents]
# AI agent panes: Claude, Codex and the others Herdr knows.
enabled = {agents_enabled}
# Any of "idle", "working", "blocked", "done".
statuses = {agents_statuses}
# Agents to stay quiet about, by Herdr's label ("claude", "codex"), not the
# name on the banner.
ignore = {agents_ignore}

[shell]
# Long shell commands, from the zsh hook or any other shell reporter.
enabled = {shell_enabled}
# How long a command runs before the zsh hook reports it.
min_seconds = {min_seconds}
# Herdr turns a finished command's "idle" into "done" when you weren't
# looking at the pane, so both are here.
statuses = {shell_statuses}
notify_on_failure_only = {notify_on_failure_only}
# Commands to stay quiet about. They end when you quit them, so a banner
# would say nothing. Matched on the command's name alone: "git" would mean
# all of git.
ignore_commands = {ignore_commands}
# Labels to treat as AI agents, or as shell commands, whatever Herdr says.
# The zsh hook leaves agents alone, since they report themselves.
# Herdr's own agents are already known and need no entry here.
known_agents_extra = {known_agents_extra}
known_agents_remove = {known_agents_remove}
"#,
        clickable_secs = n.clickable_secs,
        sound = n.sound,
        agent_logos = n.agent_logos,
        show_workspace = n.show_workspace,
        agents_enabled = a.enabled,
        agents_statuses = toml_array(a.statuses.iter().map(|s| s.as_str())),
        agents_ignore = toml_array(a.ignore.iter().map(String::as_str)),
        shell_enabled = sh.enabled,
        min_seconds = sh.min_seconds,
        shell_statuses = toml_array(sh.statuses.iter().map(|s| s.as_str())),
        notify_on_failure_only = sh.notify_on_failure_only,
        ignore_commands = toml_array(sh.ignore_commands.iter().map(String::as_str)),
        known_agents_extra = toml_array(sh.known_agents_extra.iter().map(String::as_str)),
        known_agents_remove = toml_array(sh.known_agents_remove.iter().map(String::as_str)),
    )
}

/// One line if it fits, else wrapped, a few entries per line. A JSON
/// string is also a valid TOML basic string, escapes included.
fn toml_array<'a>(items: impl Iterator<Item = &'a str>) -> String {
    let items: Vec<String> = items
        .map(|item| serde_json::to_string(item).expect("a str always serializes"))
        .collect();
    let one_line = format!("[{}]", items.join(", "));
    if one_line.len() <= 60 {
        return one_line;
    }
    let mut out = String::from("[");
    let mut line = String::new();
    for item in items {
        if !line.is_empty() && line.len() + item.len() + 2 > 72 {
            out.push_str(&format!("\n {}", line.trim_end()));
            line.clear();
        }
        line.push_str(&format!(" {item},"));
    }
    out.push_str(&format!("\n {}\n]", line.trim_end()));
    out
}
