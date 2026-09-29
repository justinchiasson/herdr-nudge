# Herdr Nudge

Mac notifications for [Herdr](https://herdr.dev). Know the moment an agent
needs you or a long command finishes, and click the notification to land on
that exact pane.

![Three notifications at the top right of a Mac desktop: Kilo blocked in the web workspace, Grok done in infra, cargo test failed in api](https://github.com/user-attachments/assets/96ab063b-acef-4020-9919-7aa8b00e8a5a)

```sh
herdr plugin install justinchiasson/herdr-nudge
```

macOS only (Apple Silicon or Intel, tested on macOS 26), Herdr 0.9.0 or
later. Nothing else to install.

## What it does

- **Agents.** A notification when an agent goes `blocked` (it's waiting for
  you) or `done`, for the agents Herdr detects: Claude Code, Codex, Kilo,
  Grok and the rest. Most show their logo.
- **Long shell commands.** With the zsh hook, a notification when a long
  command finishes, with its exit status and how long it took. "Long" means
  5 seconds or more by default, and is configurable.
- **Click to go back.** Herdr switches to the pane, in whichever workspace
  or tab it's in, and your terminal comes to the front.
- **Quiet when you're watching.** Nothing is posted for the pane you're
  looking at: focused in Herdr, with your terminal in front.
- **Cleans up after itself.** A notification goes away when its pane moves
  on (the agent starts working again, you run another command), or when the
  pane, its tab or its workspace closes. On Herdr 0.9.1 it also goes when
  you switch to the pane yourself.

| A test run fails while you're in your editor | Click the notification, and you're at the pane |
|---|---|
| ![VS Code in front, with a "cargo failed" notification for the api workspace](https://github.com/user-attachments/assets/c3a04087-a34c-474e-8277-465dd08993e8) | ![Ghostty in front, Herdr on the api workspace, showing the failed test](https://github.com/user-attachments/assets/5f6c5f35-7e83-44e1-aabf-c84f654038c0) |

## Install

1. Install the plugin:

   ```sh
   herdr plugin install justinchiasson/herdr-nudge
   ```

   Herdr shows what the plugin will run and asks first. If macOS offers to
   install the Command Line Tools, accept: Herdr needs `git` for this.
2. Wait for your first notification. macOS asks whether Herdr Nudge may send
   notifications. Click **Allow**.

That's all for agents. For long shell commands, add the
[zsh hook](#shell-commands-optional-zsh) too.

To update, run the install command again. To remove it, run
`herdr plugin uninstall herdr-nudge`, and take the zsh lines out of
`.zshrc` if you added them.

**Tips**

- Notifications stay on screen for a few seconds, then wait in
  Notification Center for an hour. To keep them on screen until you deal
  with them, set Herdr Nudge's alert style to *Persistent* in System
  Settings > Notifications (*Alerts* on older macOS).
- If every notification shows up twice, Herdr's own desktop notifications
  are on as well. [Troubleshooting](#troubleshooting) says how to turn them
  off.

## Shell commands (optional, zsh)

Herdr tells the plugin about agents by itself, but not about shell commands.
A small zsh hook does that. To turn it on:

1. Add the hook to your `.zshrc`:

   ```sh
   ~/.config/herdr/plugins/github/herdr-nudge-*/bin/herdr-nudge setup-zsh
   ```

   It shows the three lines it will add and changes nothing until you say
   yes.
2. Restart Herdr once, so the plugin writes the file those lines load:
   `herdr server stop`, then `herdr`. `setup-zsh` tells you if you need to.
3. Open a new pane. Panes that were already open don't have the hook.

To check it works, run `sleep 6` in the new pane and switch to another app.
A notification should appear when it finishes.

If you'd rather edit `.zshrc` yourself (`$ZDOTDIR/.zshrc` if you set
`ZDOTDIR`), these are the lines:

```zsh
if [[ -r ~/.local/state/herdr/plugins/herdr-nudge/herdr-nudge.zsh ]]; then
  source ~/.local/state/herdr/plugins/herdr-nudge/herdr-nudge.zsh
fi
```

Any command that runs for 5 seconds or more counts. To change that, set
`min_seconds` in the plugin's config file (see
[Configuration](#configuration)), then restart Herdr:

```toml
[shell]
min_seconds = 10
```

The hook does nothing outside a Herdr pane. Agents are left to report
themselves, whatever name you start them by, and so are the commands in
`[shell] ignore_commands` (editors, pagers, `ssh`, `tmux` and the like),
since those end when you quit them.

The notification shows the command line as you typed it, cut at 60
characters, so anything secret you put on a long-running command line ends
up in Notification Center.

## Commands

You don't need these day to day. They're for checking your setup and trying
things out. The plugin isn't on your `PATH`, so run it by its path, which
stays the same across updates:

```sh
~/.config/herdr/plugins/github/herdr-nudge-*/bin/herdr-nudge doctor
```

If you use it often, add an alias to your `.zshrc`:

```sh
alias herdr-nudge='~/.config/herdr/plugins/github/herdr-nudge-*/bin/herdr-nudge'
```

| Command | What it does |
|---|---|
| `doctor` | Checks what stops notifications: a config that doesn't load, `[agents] ignore` entries that match nothing, macOS permission and alert style, Herdr's own system toasts (which would double up), and whether `.zshrc` loads the hook. Exits 1 if something is broken. |
| `test` | Posts an agent-style notification for the pane it runs in. Switch away and click it to see where it takes you. `test --shell` posts a shell-command one. Run it inside a Herdr pane. |
| `example-config` | Writes `config.toml` with every setting at its default and a note on each. If the file already exists, it prints the example and leaves your file alone. |
| `setup-zsh` | Adds the lines from [Shell commands](#shell-commands-optional-zsh) to `.zshrc`, after showing them and asking. Does nothing if they're already there. |

You may also see the binary run with `--cleanup`, `--click` or no arguments.
Herdr runs it that way when it starts and on each event, and a notification
runs it when you click it. You never need to run those yourself.

## Configuration

Settings live in `config.toml`, in the plugin's config directory:

```sh
herdr plugin config-dir herdr-nudge
```

You don't need the file at all. [`example-config`](#commands) writes it with every
setting at its default and a note on each. These are all of them:

| Key | Default | |
|---|---|---|
| `default_terminal` | unset | Bundle id of the app a click brings forward. Unset, it's the terminal your Herdr client runs in. It's a top-level key, so it goes above the first `[section]`. |
| `[notifications] clickable_secs` | `3600` | How long a notification can still be clicked in Notification Center, in seconds. |
| `[notifications] sound` | `false` | Herdr plays its own sound for these events. `true` adds ours. |
| `[notifications] agent_logos` | `true` | The agent's logo on the right of the notification. |
| `[notifications] show_workspace` | `true` | The workspace's name under the title. |
| `[agents] enabled` | `true` | Notifications for AI agents. |
| `[agents] statuses` | `["blocked", "done"]` | Agent states that notify. Any of `idle`, `working`, `blocked`, `done`. |
| `[agents] ignore` | `[]` | Agents to stay quiet about, by Herdr's label (`"codex"`, not `"Codex"`). |
| `[shell] enabled` | `true` | Notifications for shell commands. |
| `[shell] min_seconds` | `5` | How long a command runs before it counts. |
| `[shell] statuses` | `["idle", "done"]` | Shell states that notify. Herdr turns a finished command's `idle` into `done` when you weren't looking at the pane, so both are there. |
| `[shell] notify_on_failure_only` | `false` | Only failed commands. |
| `[shell] ignore_commands` | editors, pagers, `ssh`, `tmux` and more | Commands to stay quiet about, matched on the command name. A list you set replaces the default one. |
| `[shell] known_agents_extra` | `[]` | Labels to treat as AI agents, whatever Herdr says. Herdr's own agents need no entry. |
| `[shell] known_agents_remove` | `[]` | Labels to treat as shell commands, whatever Herdr says. |

A key the plugin doesn't know makes it ignore the whole file and use the
defaults until it's fixed. [`doctor`](#commands) says so. The
`[shell]` settings reach the zsh hook only after Herdr restarts.

## FAQ

**Do I need to install anything else?**
No. Notifications are posted by
[terminal-notifier](https://github.com/julienXX/terminal-notifier), which
ships inside the plugin as an app called Herdr Nudge, for both Apple Silicon
and Intel Macs. The one thing `herdr plugin install` needs is `git`. If macOS offers
to install the Command Line Tools during the install, accept.

**Which agents does it work with?**
Any agent Herdr detects, such as Claude Code, Codex, Kilo, Grok, Gemini and
Cursor. Most show their logo. `agent_logos = false` in the
[config](#configuration) turns the logos off.

**Which terminals does it work with?**
It's tested with Ghostty. A click brings forward the app your Herdr client
runs in, so other Mac terminals should work the same way. If a click brings
up the wrong app, see [Troubleshooting](#troubleshooting).

**Do I have to use zsh?**
Only for shell-command notifications. Agent notifications work whatever
shell you use.

**Can I change how long a command has to run, which states notify, the
sound, or the logos?**
Yes. Every setting is in the [Configuration](#configuration) table.

**Why didn't I get a notification for the pane I was looking at?**
That's on purpose. If the pane is focused in Herdr and your terminal is the
app in front, you're already watching it. Switch to another pane or app and
you'll get one.

**Does it change anything on my Mac?**
Very little. The first time it runs after an install or update, it
registers its notifier app with macOS, the way opening an app from Finder
would. Some Macs won't ask for notification permission until that's done,
and running the app isn't always enough. It registers it again when Herdr
starts and when you run `doctor`. Its own files, including the zsh hook,
live in Herdr's state folder for the plugin. It only touches `.zshrc` if you
run `setup-zsh` and say yes.

**Does it send anything over the network?**
No. It only talks to Herdr and to macOS's notification service.

**Does it slow Herdr down?**
No. Herdr doesn't wait for plugins, and each event takes the plugin well
under a second.

**Does it work on Linux or Windows?**
No, it's macOS only.

## Troubleshooting

Start with [`doctor`](#commands). It checks
most of what's below and says what to fix.

**No notifications at all**
- Check that macOS allows them: System Settings > Notifications > Herdr
  Nudge, with notifications allowed.
- A Focus mode, such as Do Not Disturb, hides them.
- While you share or record your screen, macOS hides notifications unless
  "Allow notifications when mirroring or sharing the display" is on in
  System Settings > Notifications.
- Nothing is posted for the pane you're looking at (see the [FAQ](#faq)).

**Agent notifications work, but shell commands don't**
- Check that you've done all three steps in
  [Shell commands](#shell-commands-optional-zsh): the `.zshrc` lines, a
  Herdr restart since installing, and a new pane. `doctor` checks the first
  two.
- The command has to run for at least `min_seconds` (5 by default).
- Commands in `ignore_commands` (editors, pagers, `ssh` and the like) never
  notify, and with `notify_on_failure_only = true` only failed ones do.

**I received a notification for a command that was waiting on me, like 
`git commit`**
The hook times the whole command, and your editor was open for longer than
5 seconds. Raise `min_seconds`, or add the command to `ignore_commands`.
That matches on the command's name alone, so `"git"` silences every git
command.

**Every event notifies twice**
Herdr's own desktop notifications are on as well. In
`~/.config/herdr/config.toml`, set `delivery` under `[ui.toast]` to
`"herdr"` (in-app only) or `"off"`, then run `herdr server reload-config`.
They're off unless you changed it.

**Clicking brings up the wrong window, tab or app**
A click brings your terminal to the front, but it can't pick the window or
tab yet. macOS brings forward the window you used last, with whatever tab it
was showing. If it's the wrong app entirely, set `default_terminal` to
your terminal's bundle id, which this prints (Ghostty as the example):

```sh
osascript -e 'id of app "Ghostty"'
```

**Clicking a notification does nothing**
It's older than `clickable_secs` (an hour by default).

**Notifications disappear too quickly**
Set Herdr Nudge's alert style to *Persistent* in System Settings >
Notifications.

**My settings have no effect**
A key the plugin doesn't know, such as a typo, makes it ignore the whole
file. [`doctor`](#commands) points at the line. `[shell]` settings only reach the hook
after Herdr restarts. `default_terminal` has to go above the first
`[section]`.

If you're still stuck, open an issue with the output of [`doctor`](#commands) and of
`herdr plugin log list --plugin herdr-nudge --limit 20`.

## Known limits

- A `done` notification stays up if you come back to its pane by switching
  apps, with the pane already selected in Herdr. Herdr sends plugins no event
  for that. It goes when you click it, when the pane changes state again, or
  after an hour.
- A click brings your terminal app to the front, but not the window or tab
  running Herdr. With several windows open, macOS brings forward the one you
  used last. With several tabs, the one that was showing stays showing.
- If you move a pane to another workspace, a notification it already had
  doesn't clear by itself.

## Development

Building from source needs Rust 1.88 or later. `cargo test` runs the
tests. The manifest runs the committed universal binary in `bin/`, so after
changing the source, `tools/build-bin.sh` rebuilds it (needs both Rust
targets: `rustup target add x86_64-apple-darwin aarch64-apple-darwin`), and
it's committed with the change. After a Herdr upgrade, see
`tests/fixtures/README.md`.

## License

MIT, see [LICENSE](LICENSE).
[terminal-notifier](https://github.com/julienXX/terminal-notifier) is MIT too,
and its licence is in
[vendor/terminal-notifier-LICENSE.md](vendor/terminal-notifier-LICENSE.md).

The Herdr logo and the agent logos belong to their owners, and this plugin
isn't affiliated with any of them. [NOTICE.md](NOTICE.md) lists where each
came from.
