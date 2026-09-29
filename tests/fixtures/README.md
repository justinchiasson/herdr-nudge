# Test fixtures

Every file here was captured from Herdr 0.9.0 or 0.9.1 on the dev machine,
some from a throwaway server there (the 0.9.0 close events, finding 17, and
any a later `verify.py --record` adds, below). None is hand-written. Don't edit them; recapture instead. Each event fixture
says which Herdr sent it (`herdr_version`, from the raw log it came from),
and `cli/` and `socket/` captures record `herdr --version`. The two versions
behave differently in places (finding 3), so both sets stay and both are
replayed.

```
raw/                 probe logs, exactly as tools/probe/dump.sh wrote them
events/<category>/   one plugin invocation each, generated from raw/
cli/                 read-only `herdr` queries: argv, exit code, stdout, stderr
socket/              one Herdr socket request and its reply line
sys/                 macOS tools (lsappinfo, open): argv, exit code, output
```

Regenerate with `tools/fixtures/extract.py`:

```sh
python3 tools/fixtures/extract.py list [LOG]    # index a probe log, with its marks
python3 tools/fixtures/extract.py write         # regenerate events/ from raw/
python3 tools/fixtures/extract.py cli NAME ARGS # capture `herdr ARGS` into cli/
python3 tools/fixtures/extract.py socket NAME METHOD PARAMS_JSON  # into socket/
python3 tools/fixtures/extract.py sys NAME PROG ARGS  # into sys/
python3 tools/fixtures/extract.py scrub         # redact the captures in place
```

## After a Herdr upgrade

The tests replay these files, so they pass whatever a new Herdr does. To see
what actually changed, run

```sh
python3 tools/fixtures/verify.py            # exit 1 if anything moved
python3 tools/fixtures/verify.py --record   # also save the new version's output where it moved
```

It starts a throwaway Herdr server under `/tmp/herdr-nudge-verify` (your own
server is never contacted), sets up panes the way the newest captures had
them, runs the same queries, and prints every key path, type or error code
that differs from the capture for the installed version, or from the newest
older one when there isn't one yet. `sys/` captures are run again on this
Mac the same way. Values that only reflect state (ids, titles, paths,
timestamps, notification settings) are ignored. The older captures of a name
aren't always the same scenario (`cli/pane-get-unfocused` is a plain shell
on 0.9.0 and a claimed pane on 0.9.1), so running this on an older Herdr can
report changes that aren't there. The script lists the captures it skips and
why, and fails on any capture it doesn't know about.

`agent-manifests` needs a network: a new server fetches the manifests a few
seconds after it starts.

So a version gets its own capture (`<name>-<version>.json`) only where its
output differs from the older one. A capture that still matches already
describes the newer version. Captures made by `--record` come from the
throwaway server, with a stand-in `claude` script for the agent.

When a Herdr version stops being supported, delete the captures that only it
has, and any test that replays them, in the same change. Otherwise
nothing here ever shrinks.

## Redaction

These files are published, so the capturing machine's identity is replaced
before they are committed. `scrub` rewrites `raw/`, `cli/`, `socket/` and
`sys/` in place, and `write` then regenerates `events/` from the redacted
logs. It also runs automatically after every capture, and it is idempotent.

| Real | In the fixtures |
|---|---|
| the capturing user's account name | `dev` (so `/Users/dev/…`) |
| the machine's hostname | `dev-mac` |
| each agent session UUID | `00000000-0000-4000-8000-0000000000NN` |

No mapping from a real id to its placeholder is stored anywhere — that file
would be the leak. Numbering continues past the highest placeholder already
present, so a later capture cannot reuse an earlier session's number.

Redaction is the one edit a `raw/` log may receive. It substitutes within
lines and never adds or removes any, so the line numbers in `SELECTIONS`
stay valid.

Deliberately **not** redacted, because they are fixture content the code is
tested against: pane, tab and workspace ids, timestamps, and terminal titles
naming this project's own work (notification text is composed from `title`
and `terminal_title_stripped`).

**Check before committing a new capture:** `grep -ri "<your username>" tests/`
should return nothing.

To add event fixtures: mark each step while capturing (below), copy the new
part of `tools/probe/events.log` into a new file in `raw/` (never edit an
existing one), add the log's Herdr version to `HERDR_VERSIONS` and its
entries to `SELECTIONS` in the script, then run `scrub` and `write`.

## Provenance: manual vs programmatic

Manual actions (a person clicking or typing in the Herdr TUI, or a real
agent's own hooks) and programmatic ones (`herdr` CLI, socket API) have
produced different results. Every fixture records which it was, plus the
nearest log mark. Mark each step before acting:

```sh
tools/probe/mark.sh "[manual] clicked from w3:p1 to w3:p3"
tools/probe/mark.sh "[programmatic] herdr tab focus w3:t2"
```

A programmatic result is never evidence for manual behaviour, or the reverse.

## Event fixture format

```json
{
  "source": "tests/fixtures/raw/<log>:<line>",
  "herdr_version": "0.9.0 | 0.9.1",
  "captured_at": "HH:MM:SS",
  "provenance": "manual | programmatic | unknown",
  "mark": "the last mark.sh note before this event, or null",
  "why": "what this case covers",
  "event": "pane.agent_status_changed",
  "event_json": "<HERDR_PLUGIN_EVENT_JSON, verbatim>",
  "env": { "HERDR_...": "<verbatim>", "HERDR_PLUGIN_CONTEXT_JSON": "<verbatim>" }
}
```

A `socket/` or `sys/` capture is only safe to repeat: both run against the
live machine, and `socket` talks to the running server. `sys` stores the
bare program name, because tests match a recording by file name while the
code calls a tool by absolute path.

`event_json` and the env values are kept as strings so a test can hand the
plugin the same bytes Herdr did. Plugin root, state and config paths in `env`
belong to the probe; tests should override them.

## Catalogue

`m` = manual, `p` = programmatic, `?` = unknown.

| File | | Covers |
|---|---|---|
| `agent/blocked` | m | Claude `blocked`, user on that pane |
| `agent/blocked-user-elsewhere` | m | Claude `blocked` 24 s after the user left the pane |
| `agent/done` | m | unwatched completion |
| `agent/done-user-elsewhere` | m | `done` ~20 s after the user left the pane |
| `agent/idle-watched-completion` | m | watched completion arrives as `idle` |
| `agent/idle-first-after-claim` | m | first status after Claude claims a pane |
| `agent/working` | m | never notifies |
| `agent/status-unknown-no-agent-field` | m | after Claude `/exit`: `agent` missing |
| `shell/idle-with-title-labels` | p | watched; `title`, `display_agent`, `state_labels` |
| `shell/idle-without-working` | p | `idle` with no preceding `working` stays `idle` |
| `shell/done-unwatched-failed` | p | reported `idle` arrives as `done`, `idle=failed` kept |
| `shell/done-unwatched-after-handover` | p | the same, with `idle=done` |
| `shell/working-metadata-update` | p | metadata alone emits a status event, so the same status arrives twice |
| `shell/blocked-with-title-labels` | p | reporter sends `blocked` |
| `shell/blocked-bare` | p | optional fields missing |
| `shell/blocked-unfocused-pane` | p | pane isn't its tab's focused pane |
| `shell/status-unknown-on-release` | p | `agent_status: "unknown"` |
| `detected/shell-claim` | p | no `released` field |
| `detected/shell-release` | p | `released: true`, `final_status` |
| `detected/agent-claim-after-shell-release` | m | shell → agent handover |
| `detected/agent-release-on-exit` | m | Claude `/exit`: `final_status: "idle"` |
| `detected/shell-claim-after-agent-exit` | p | agent → shell handover |
| `lifecycle/pane-closed` | m | agent pane closed with no release first |
| `lifecycle/pane-created` | ? | nested `pane` object |
| `lifecycle/tab-created` | p | `tab create --no-focus` |
| `focus/tab-focus-{tab,pane,workspace}-focused` | p | one `herdr tab focus` burst |
| `focus/tab-focus-back-pane-focused` | p | the return burst, in a different order |
| `focus/socket-pane-focus-{pane,tab,workspace}-focused` | p | socket `pane.focus`, the call a click makes: also a burst of three, so a click's own focus event can dismiss the notification |
| `focus/manual-tab-click-{pane,tab,workspace}-focused` | m | 0.9.1: the user clicked another tab, and all three arrive |
| `focus/manual-pane-click-pane-focused` | m | 0.9.1: the user clicked the other pane in the same tab |
| `focus/manual-workspace-click-pane-focused` | m | 0.9.1: the user clicked another workspace |
| `focus/socket-pane-focus-moved-pane-focused` | p | 0.9.1: socket `pane.focus` onto a pane in another tab |
| `shell/done-focused-terminal-in-background` | p | 0.9.1: `done` for a pane focused in Herdr while another app was in front |
| `lifecycle/pane-closed-by-cli` | p | 0.9.1: `herdr pane close` on a pane in a background tab |
| `lifecycle/tab-closed` | m | 0.9.1: the user closed a tab with two panes. `tab_id` and `workspace_id` only, no `pane.closed` |
| `lifecycle/workspace-closed` | m | 0.9.1: the user closed a workspace with two tabs. Counts but no ids of what was in it, no `tab.closed` or `pane.closed` |
| `lifecycle/tab-closed-by-cli` | p | 0.9.1: `herdr tab close` on a background tab with two panes |
| `lifecycle/workspace-closed-by-cli` | p | 0.9.1: `herdr workspace close` on a background workspace, two tabs, three panes |
| `lifecycle/tab-closed-by-pane-move` | p | 0.9.1: `herdr pane move` took a tab's only pane into another tab, which closed the tab while the pane lives on |
| `lifecycle/{tab,workspace}-closed-by-cli-0.9.0` | p | 0.9.0: the same two closes, on a throwaway server |
| `lifecycle/tab-closed-by-pane-move-0.9.0` | p | 0.9.0: the same, the pane moved to another workspace, where it got a new id |
| `cli/agent-get-with-session` | m | `agent_session` present: an agent |
| `cli/agent-get-reported-no-session` | p | agent label, no `agent_session`: a shell command |
| `cli/agent-get-plain-shell` | – | `agent_not_found` on stderr, exit 1 |
| `cli/agent-manifests` | – | the agent labels Herdr detects by itself |
| `cli/agent-manifests-0.9.1` | – | the same on 0.9.1, which adds `letta` |
| `cli/pane-get-focused` | – | `focused: true` for the pane the user is on |
| `cli/pane-get-unfocused` | – | `focused: false`, and it works on a plain shell pane |
| `cli/pane-get-unfocused-0.9.1` | p | 0.9.1, a pane claimed by `make`: the same fields as 0.9.0 |
| `cli/pane-get-reported-no-session` | p | a pane claimed by `pane report-agent`: `agent`, `title` and `state_labels`, no `agent_session` |
| `cli/pane-get-after-release` | p | the same pane after `release-agent`: no `agent`, `unknown` status, `title` and `state_labels` left behind |
| `cli/pane-get-detected-alias` | p | 0.9.0, throwaway server: a script named `cursor-agent` (an alias in Herdr's manifests) running in the pane. `agent` is `cursor`, Herdr's label, and there is no `agent_session` |
| `cli/pane-get-not-found` | p | 0.9.0: `pane_not_found` on stderr, exit 1, nothing on stdout |
| `cli/pane-list` | – | every pane; exactly one has `focused: true` |
| `cli/plugin-config-dir` | – | where Herdr keeps this plugin's config: a bare path, not JSON |
| `cli/workspace-get` | – | 0.9.1: a workspace's `label`, which a pane's own environment doesn't carry |
| `cli/workspace-get-not-found` | – | 0.9.1: `workspace_not_found` on stderr, exit 1 |
| `cli/workspace-get-empty-label` | – | 0.9.1: made with `workspace create --label ""`, and `label` comes back `""`. Without `--label`, Herdr names it after the cwd |
| `socket/pane-focus-ok` | p | the focus call a click makes; reply is `{"id","result"}` |
| `socket/pane-focus-not-found` | p | `pane_not_found`, the error reply shape |
| `sys/lsappinfo-front` | – | the frontmost app's ASN |
| `sys/lsappinfo-bundleid-ghostty` | – | that ASN's bundle id |
| `sys/lsappinfo-bundleid-gone` | – | an app that has quit: `[ NULL ]`, still exit 0 |
| `sys/open-bundle-ghostty` | – | `open -b` raising the terminal a click goes to: exit 0, no output |
| `sys/open-bundle-unknown` | – | `open -b` with a bundle id nothing has: exit 1, reason on stderr |
| `sys/defaults-appearance-light` | – | `defaults read -g AppleInterfaceStyle` in light mode: exit 1, the key doesn't exist |
| `sys/defaults-appearance-dark` | – | the same in dark mode: exit 0, `Dark` |
| `sys/pgrep-herdr-two-sessions` | – | every `herdr` process: the default session's server (86129) and two clients (86128 in Ghostty, 21836 in iTerm), and a second session `nudge-capture` with its server (40823) and three clients (40822, 40852, 40869) |
| `sys/pgrep-herdr-none` | – | the same query matching nothing: exit 1, no output. Captured with a pattern that matches no process, so tests replay it under the real argv |
| `sys/lsof-capture-session-clients` | – | unix sockets of both servers and every client. Each server holds its own `herdr.sock` under that name, and each client connects to a `herdr-client.sock` socket of its own server |
| `sys/lsof-capture-session-one-client` | – | the same for the `nudge-capture` server and its Ghostty client alone |
| `sys/ps-env-capture-session-clients` | – | the three `nudge-capture` clients' command lines and environments: Ghostty, iTerm, none |
| `sys/ps-env-capture-session-one-client` | – | the Ghostty one alone |
| `sys/lsappinfo-visible-process-list` | – | visible apps, most recently used first: Ghostty, then Chrome, then iTerm |
| `sys/lsappinfo-find-{ghostty,iterm}` | – | a running app's ASN, by bundle id |
| `sys/lsappinfo-find-not-running` | – | an app that isn't running: exit 0, no output |
| `sys/pgrep-herdr-three-servers` | – | 0.9.1: the default session's server (85985) and client (85984), plus two throwaway sessions' servers, `nudgexdg` (15337) and `nudgeplain` (16451), with no clients |
| `sys/ps-env-server-xdg` | – | the `nudgexdg` server's environment, started with `XDG_STATE_HOME` set. Its hooks' state went under that directory, not `~/.local/state` |
| `sys/ps-env-server-plain` | – | the `nudgeplain` server's, started without it |
| `sys/pgrep-herdr-documents-servers` | – | 0.9.1: the default session's server and client, plus `nudgedocs` (24641) and `nudgenohome` (24642) |
| `sys/ps-env-server-documents` | – | `nudgedocs`: `HOME=/tmp/hnd/a`, `XDG_STATE_HOME` in that home's `Documents` |
| `sys/ps-env-server-no-home` | – | `nudgenohome`: no `HOME` at all, `XDG_STATE_HOME=/tmp/hnd/b/Documents/state` |
| `sys/terminal-notifier-diagnose` | – | our bundle's `-diagnose` with notifications allowed and alert style Banners |
| `sys/osascript-register-bundle` | – | `LSRegisterURL(<our bundle>, true)` through `osascript`: prints `0`, exit 0 |
| `sys/osascript-register-missing` | – | the same for a path that doesn't exist: prints `-43`, and still exit 0 |

The `pgrep-herdr-*` captures record the pattern from before it took a
leading `-` (`-herdr`, iTerm2's custom shell). Tests replay them under
today's pattern (`Recorded::pgrep_herdr`), and every process they list
matches either way.

The `nudge-capture` session was made for these captures and deleted
afterwards. Its server and clients were started from a pty with only
`HOME`, `PATH`, `TERM` and `LANG` set, plus `__CFBundleIdentifier` and
`TERM_PROGRAM` copied from a real Ghostty or iTerm shell, because `ps -E`
prints a process's whole environment and a real terminal's holds tokens.
So the terminals in them are the right values for those apps, but the
clients did not really run inside the apps. The default session's
clients have real environments, which is why no `ps -E` capture includes
them.

The `nudgexdg` and `nudgeplain` servers were started headless (`herdr
server` with `HERDR_SESSION` set) under `env -i` with only `HOME`, `USER`,
`PATH` and, for `nudgexdg`, `XDG_STATE_HOME`, for the same reason. Both
sessions were deleted afterwards. `nudgedocs` and `nudgenohome` were
started the same way, with fake homes under `/tmp/hnd` so no real
`~/Documents` was touched, and killed after the capture.

## Findings (2026-09-18 captures)

Each finding lists the log marks that back it. Several contradict what the
documentation and other plugins assume about Herdr, so they are recorded here
with their evidence rather than only in code comments.

1. **`focused_pane_id` is the event's own pane, not where the user is.**
   Manual: Claude in `w3:p4` blocked 24 s after the user clicked to `w3:p3`
   (and was `done` ~20 s after, in an earlier run). Both events said
   `focused_pane_id: "w3:p4"`. Programmatic runs agree, including a pane
   that wasn't even its tab's focused pane. Across all 196 status events in
   `raw/` it never differs from the event's pane, so it can't tell us
   whether the user is watching.
   **Use `herdr pane get <pane_id>` instead:** its `focused` field does track
   manual navigation, across panes, tabs and workspaces (polled once a second
   through a manual run, every move seen), exactly one pane is focused
   server-wide, and unlike `agent get` it works on plain shell panes. This is
   what `herdr-focus-notify` does, via `pane list`.
2. **Herdr knows where the user is anyway: an unwatched completion becomes
   `done`.** True for agents (manual) and for reported shell `idle`
   (programmatic), provided a `working` came first. Without a preceding
   `working` the `idle` stays `idle`. A `[shell] statuses` of `idle` alone
   would miss the unwatched case, which is the one that matters, so `done`
   is in the default set too. `state_labels` survive the change.
3. **On 0.9.0, manual navigation emits no focus events.** Mouse and
   keyboard, across panes, tabs and workspaces: zero
   `pane/tab/workspace.focused`. `herdr tab focus` emits all three in the
   same second, in varying order, and `workspace.focused` fires even when
   the workspace doesn't change. **0.9.1 changed this**, see finding 11.
   `tests/focus_events.rs` checks both.
4. **`--seq` persists per pane and source, across a release.** Reusing
   `--seq 1` after an earlier claim silently dropped the `working` report.
   The zsh hook's `--seq` must keep increasing across shells.
5. **Metadata persists across a release.** A new claim's first event carried
   the previous claim's `title` and `state_labels`. The hook must always set
   or clear metadata.
6. **`report-metadata` alone emits `pane.agent_status_changed`**, with an
   unchanged status, so the same (pane, status) arrives repeatedly and
   notifications have to be deduplicated.
7. **Claude `/exit` sends a release; closing its pane doesn't.** `/exit` gives
   `agent_detected {released: true, final_status: "idle"}` then an `unknown`
   status with no `agent` field. `pane.closed` arrived with no release first.

## Findings (2026-09-20 captures)

8. **`pane get` carries both agent-or-shell signals, so `agent get` is never
   needed.** On a pane claimed with `pane report-agent --agent make`,
   `pane get` returned `agent: "make"` with no `agent_session` key at all —
   the same two fields `agent get` returns for a claim like it
   (`cli/agent-get-reported-no-session`, a different pane on a different
   day), from the same query that reports `focused`. A pane that never had
   an agent still gets an answer from `pane get`, where `agent get` fails
   with `agent_not_found`. `cli/pane-get-reported-no-session`, captured with:

   ```sh
   seq=$(python3 -c "import time; print(int(time.time() * 1000))")
   herdr pane report-agent w3:p2 --source herdr-nudge-capture \
       --agent make --state working --seq $seq
   herdr pane report-metadata w3:p2 --source herdr-nudge-capture \
       --title "make test · exit 0 · 2m11s" --state-label "idle=finished" \
       --seq $((seq + 1))
   python3 tools/fixtures/extract.py cli pane-get-reported-no-session \
       pane get w3:p2
   ```

9. **`release-agent` clears the agent label, and it needs a `--seq`.**
   After a release the pane has no `agent` at all and `agent_status` back to
   `unknown`, and `agent get` returns to `agent_not_found`
   (`cli/pane-get-after-release`). The `title` and `state_labels` stay behind,
   like finding 5 says. **A release whose `--seq` is not higher than the last
   report is dropped in silence** — exit 0, no output, nothing changes.
   Reproduced three times: the first capture of this fixture recorded a
   release that never happened, because it passed no `--seq` at all. The zsh
   hook releases on every `preexec` and on `zshexit`, so both have to carry a
   `--seq` like the reports do.

   `revision` is no help in telling whether any of this landed: it stayed at
   43 across a claim, a report, a dropped release and a real one.

10. **A reporter could bind a session if it wanted one.**
    `pane report-agent` takes `--agent-session-id` and
    `--agent-session-path`, so the missing `agent_session` is a fact about
    shell hooks that don't pass them, not something Herdr enforces. Ours
    doesn't pass them.

## Findings (2026-09-23 captures, Herdr 0.9.1)

`raw/events-2026-09-23-herdr-0.9.1.log`, after the upgrade and a server
restart.

11. **Manual navigation sends focus events.** Eight mouse clicks: to another
    tab by the tab bar and back, the same by the agent list on the left and
    back, to the other pane in the tab and back, to another workspace and
    back. Every one sent `pane.focused`, `tab.focused` and
    `workspace.focused` within the same second, in varying order. The
    keyboard wasn't tried.
12. **The context can't tell a person from a script.** The manual focus
    events above carry `invocation_source: "api"`, the same as every other
    event in both versions. The context has the same fields as on 0.9.0.
13. **Coming back to the terminal app sends nothing.** Cmd-Tab to Chrome
    and back with the pane focused throughout: no focus event.
14. **A pane that finished while the terminal was in the background, then
    seen, sends nothing either.** `w3:p9` was focused in Herdr by
    `herdr tab focus` while Chrome was in front, and a reported `idle`
    arrived as `done` (`shell/done-focused-terminal-in-background`). The
    user came back with Cmd-Tab: `herdr pane get` polled once a second read
    `done` while Chrome was in front and `idle` from the moment Ghostty
    was, but no status event followed. The same as on 0.9.0, where it was
    seen with a real Claude pane.
15. **Socket `pane.focus` on the pane that already has focus sends
    nothing.** Onto a pane in another tab it sends all three, as on 0.9.0.
16. **Pane ids are not decimal.** The next pane created in `w3` after `p9`
    was `w3:pA`. Nothing here parses them.

## Findings (2026-09-26 captures, both versions)

`raw/events-2026-09-26-herdr-0.9.1-closes.log` (the dev server) and
`raw/events-2026-09-26-herdr-0.9.0-closes.log` (a throwaway headless
server under a scratch `HOME` in `/tmp`, running the downloaded 0.9.0
release binary, sha256 checked against GitHub's; the binary's path in its
`HERDR_BIN_PATH` is a temp directory, and `scrub` took a UUID in that path
for an agent session).

17. **Closing a tab or a workspace sends no `pane.closed`.** By hand or by
    CLI, on both versions. `tab.closed` has `tab_id` and `workspace_id`;
    `workspace.closed` has `workspace_id` and a `workspace` object with
    counts. Neither lists the panes, and a workspace close sends no
    `tab.closed` either. Closing a workspace's only tab sends
    `workspace.closed` then `tab.closed`.
18. **A tab can close with its pane still alive.** `herdr pane move` of a
    tab's last pane into another tab closes the old tab (`tab.closed`, and
    the move's reply has `closed_tab_id`). So a tab id says nothing about
    which panes are gone.
19. **A pane moved to another workspace gets a new id** (`w1:p4` → `w4:p2`
    on 0.9.0, `w7:p2` → `w8:p2` on 0.9.1; `pane.moved` has
    `previous_pane_id`). Within a workspace it keeps its id.
20. **By the time a close hook runs, `herdr pane list` no longer has the
    closed panes.** Checked on 0.9.1 with a throwaway plugin that ran it
    from its `tab.closed` and `workspace.closed` hooks, three of each: none
    listed a closed pane. Not kept as a fixture.

## Findings (2026-09-26, agent aliases, both versions)

Throwaway headless servers under a scratch `HOME` in `/tmp`, one per
version (0.9.0 the downloaded release binary, sha256 checked against
GitHub's). Stand-in scripts named after the aliases, as `verify.py` does
for `claude`. Only `cli/pane-get-detected-alias` and
`cli/pane-get-not-found` are kept; the rest was read from `pane get` and
isn't a fixture.

21. **Herdr detects an agent by any of its manifest aliases, and labels it
    with the manifest's id.** `cursor-agent` → `cursor`, `claude-code` →
    `claude`, `kiro-cli` → `kiro`. The aliases come from the manifests
    Herdr fetches after it starts (`aliases =` in
    `agent-detection/remote/*.toml` under its state dir), which
    `agent-manifests --json` doesn't list. Detection showed up in `pane get`
    0.1 s after the command started, 0.6 s once on 0.9.0. No
    `agent_session`: only an integration binds one. A script not named
    after an agent stayed unlabelled for 12 s.
22. **`pane get` doesn't say who reported an agent.** A pane a shell hook
    claimed (`make`) and a pane Herdr detected (`cursor`) have the same
    fields; only the label differs.
23. **Whoever labels a pane first keeps the label.** A report from another
    source on a detected pane changed its status and title, but `agent`
    stayed `cursor`. The other way round, a detected program started while
    a report held the pane stayed hidden behind the reported label, and
    showed up 1.3–1.8 s after that report was released.
24. **The label goes when the detected program exits**, within 0.1–0.3 s.
25. **`pane get` against a stopped server never returns.** With the server
    under SIGSTOP it was still waiting after 40 s, on both versions. With
    no server at all it fails at once (`server_not_running`, exit 1).
26. **0.9.2 releases a reported shell command by itself once its prompt is
    back**, in the same second as the `done`
    (`shell/released-by-herdr-after-done-0.9.2`). The event looks like a
    reporter's own release on every version: `unknown`, still naming the
    agent. On 0.9.0 and 0.9.1 that release only came with the next
    command, and on 0.9.2 the hook's own release then does nothing. So the
    hook clears the pane's title and labels after it releases, and on a
    pane nobody claims Herdr answers with an `unknown` naming no agent
    (`shell/cleared-on-next-command-0.9.2`). Checked with the hook in a
    throwaway server on all three versions (`raw/events-2026-09-29-herdr-0.9.2-clear.log`
    is the 0.9.2 run).
27. **`notification.show` skips the check that loses 0.9.2's shell sound.**
    Herdr shows its toast and plays its `done` sound only if the pane is
    still `done` about a second later (`[ui.toast] delay_seconds`), and a
    released shell command isn't. A `notification.show` doesn't point at a
    pane and isn't checked, so it gets Herdr's own sound and toast by the
    user's Herdr settings. With no client attached the server answers
    `shown: false` (`socket/notification-show`, same on 0.9.0 and 0.9.2),
    and it takes one a second across the server (`rate_limited`). Read in
    Herdr's source, not measured.

## Still missing

- `agent get` for a **real** agent without a Herdr integration. The
  shell-reporter case (`cli/agent-get-reported-no-session`) covers the same
  shape: label present, `agent_session` absent.
- Manual navigation by keyboard on 0.9.1. Only the mouse was captured.
- `blocked` while the user is in another **tab** or **workspace**, manual.
  Skipped: finding 1 already rules out `focused_pane_id`.
- An `agent explain --file` replay, which would exercise agent detection
  offline against a captured screen.
