#!/usr/bin/env python3
"""Check real banners against real events, with a real terminal in or out of front.

    python3 tools/live-check.py wait-away    # returns once the terminal stays in the back
    python3 tools/live-check.py away         # the terminal in the back
    python3 tools/live-check.py await-click  # waits until someone clicks the banner
    python3 tools/live-check.py click-only   # one banner to click, instead of `away`
    python3 tools/live-check.py front        # the terminal in front, hands off the keyboard
    python3 tools/live-check.py clean        # only after a run that stopped early

cargo test replays saved output and verify.py re-runs commands, so neither sees
what Herdr's events do. On Herdr 0.9.2 both were happy while every shell banner
vanished the moment it appeared.

This runs against the server the calling pane belongs to. A scratch server has
no attached client, so nothing on it can count as being watched. All the work
happens in a temporary workspace, and that's the only thing it closes. It posts
real banners, and Herdr plays real sounds.

It never brings the terminal forward. Someone typing in another app would have
their keys land in a test pane, so a stage that finds the terminal in the wrong
place stops. A machine can't click a banner or hear a sound, so whoever runs
this has to ask about those.

Exit codes: 0 every check passed, 1 a check failed, 3 stopped because the
terminal was in the wrong place, which says nothing about the plugin.
"""
import glob
import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
HERDR = os.environ.get("HERDR_BIN_PATH") or shutil.which("herdr") or os.path.expanduser("~/.local/bin/herdr")
PLUGIN_BIN = os.path.join(REPO, "bin", "herdr-nudge")
NOTIFIER = os.path.join(REPO, "vendor", "HerdrNudge.app", "Contents", "MacOS", "terminal-notifier")
STATE_ROOT = os.environ.get("XDG_STATE_HOME") or os.path.expanduser("~/.local/state")
JOBS = os.path.join(STATE_ROOT, "herdr", "plugins", "herdr-nudge", "jobs")
RUN_FILE = os.path.join(tempfile.gettempdir(), "herdr-nudge-live-check.json")
TERMINAL = os.environ.get("LIVE_CHECK_TERMINAL", "com.mitchellh.ghostty")
LABEL = "nudge-check"

# Long enough to pass the hook's default min_seconds, which is 5.
SLEEP_S = 8
SLEEP = f"sleep {SLEEP_S}"
# A new pane's zsh needs this long to get through its .zshrc.
SHELL_START_S = 2.5
# Long enough for a late release from Herdr to have taken a banner down.
STILL_UP_S = 5
CLICK_TIMEOUT_S = 540
# `away` takes about a minute, and a quick look at another app isn't leaving.
SETTLED_AWAY_S = 10

failures = []


class Stop(Exception):
    """The terminal isn't where this stage needs it."""


def herdr(*args):
    out = subprocess.run([HERDR, *args], capture_output=True, text=True, timeout=30)
    try:
        return json.loads(out.stdout)
    except ValueError:
        return {"raw": out.stdout, "stderr": out.stderr, "code": out.returncode}


def call(method, params):
    with socket.socket(socket.AF_UNIX) as s:
        s.settimeout(5)
        s.connect(os.environ["HERDR_SOCKET_PATH"])
        s.sendall((json.dumps({"id": "live-check", "method": method, "params": params}) + "\n").encode())
        reply = b""
        while not reply.endswith(b"\n"):
            chunk = s.recv(65536)
            if not chunk:
                break
            reply += chunk
    return json.loads(reply)


def focus(pane):
    return call("pane.focus", {"pane_id": pane})


def pane_info(pane):
    return herdr("pane", "get", pane).get("result", {}).get("pane", {})


def focused_panes():
    panes = herdr("pane", "list").get("result", {}).get("panes", [])
    return [p["pane_id"] for p in panes if p.get("focused")]


def run_in(pane, line):
    return herdr("pane", "run", pane, line)


def now_ms():
    return int(time.time() * 1000)


def wait(condition, timeout, step=0.5):
    end = time.time() + timeout
    while time.time() < end:
        value = condition()
        if value:
            return value
        time.sleep(step)
    return None


def front_app():
    asn = subprocess.run(["lsappinfo", "front"], capture_output=True, text=True).stdout.strip()
    out = subprocess.run(["lsappinfo", "info", "-only", "bundleid", asn], capture_output=True, text=True).stdout
    return out.strip().split("=")[-1].strip('"')


def need_terminal_behind():
    app = front_app()
    if app == TERMINAL:
        raise Stop(f"{TERMINAL} is in front, and this stage needs it behind another app")
    return app


def need_terminal_in_front():
    app = front_app()
    if app != TERMINAL:
        raise Stop(f"{app} is in front, and this stage needs {TERMINAL}")


def banners(pane):
    out = subprocess.run([NOTIFIER, "-list", f"herdr-nudge-{pane}"], capture_output=True, text=True, timeout=10)
    lines = [line for line in out.stdout.splitlines() if line.strip()]
    return lines[1:] if lines and lines[0].startswith("GroupID") else lines


def remove_banner(pane):
    subprocess.run([NOTIFIER, "-remove", f"herdr-nudge-{pane}"], capture_output=True, timeout=10)


def jobs(pane):
    found = []
    for path in glob.glob(os.path.join(JOBS, "*.json")):
        try:
            with open(path) as f:
                if json.load(f).get("pane_id") == pane:
                    found.append(os.path.basename(path)[: -len(".json")])
        except (OSError, ValueError):
            pass
    return found


def plugin_log(pane, since):
    entries = herdr("plugin", "log", "list", "--plugin", "herdr-nudge", "--limit", "60")
    picked = []
    for entry in entries.get("result", {}).get("logs", []):
        if entry.get("started_unix_ms", 0) >= since and f"pane={pane} " in entry.get("stderr", ""):
            lines = [line.replace("herdr-nudge: ", "") for line in entry["stderr"].splitlines()]
            picked.append((entry["started_unix_ms"] - since, entry["event"], lines))
    return sorted(picked, key=lambda item: item[0])


def log_line(pane, since, start):
    for _, _, lines in plugin_log(pane, since):
        for line in lines:
            if line.startswith(start):
                return line
    return None


def wait_for_log(pane, since, start, timeout):
    return wait(lambda: log_line(pane, since, start), timeout)


def check(name, ok, pane=None, since=None):
    print(f"{'ok  ' if ok else 'FAIL'}  {name}")
    if not ok:
        failures.append(name)
        if pane:
            print(f"      banners: {banners(pane)}  jobs: {jobs(pane)}  front: {front_app()}")
            for ms, event, lines in plugin_log(pane, since or 0):
                print(f"      +{ms / 1000:.1f}s {event}")
                for line in lines:
                    print(f"        {line}")


def load_run():
    try:
        with open(RUN_FILE) as f:
            return json.load(f)
    except (OSError, ValueError):
        return None


def save_run(run):
    with open(RUN_FILE, "w") as f:
        json.dump(run, f)


def await_posted_away(pane, since):
    """The Posted line, or Stop if the terminal came forward while it ran."""
    line = wait_for_log(pane, since, "Posted", 20)
    if not line and log_line(pane, since, "Watching"):
        raise Stop("the terminal came forward during this check, so it counted as watched")
    return line


def check_failed_command(pane):
    need_terminal_behind()
    since = now_ms()
    run_in(pane, f"{SLEEP}; false")
    line = await_posted_away(pane, since)
    check("failed command, terminal behind: posts `sleep · failed`", line and "sleep · failed" in line, pane, since)
    shown_nothing = log_line(pane, since, "herdr showed nothing")
    if shown_nothing:
        print(f"note  {shown_nothing}. Herdr won't play its sound for this one.")
    # Herdr 0.9.2 started releasing a reported command as soon as its prompt
    # came back, and the banners went down with that release.
    time.sleep(STILL_UP_S)
    check(f"that banner is still up {STILL_UP_S} s later", bool(banners(pane)), pane, since)


def check_next_command_clears(pane):
    since = now_ms()
    run_in(pane, "true")
    gone = wait(lambda: not banners(pane), 8)
    check("next command, no click: banner goes away", bool(gone) and not jobs(pane), pane, since)


def check_omz_reload(pane):
    if "omz-present=1" not in shell_says(pane, 'print -r -- "omz-present=${+functions[omz]}"'):
        print("skip  omz reload: no oh-my-zsh in this shell")
        return
    run_in(pane, "omz reload")
    time.sleep(SHELL_START_S)
    need_terminal_behind()
    since = now_ms()
    run_in(pane, SLEEP)
    check("after `omz reload`: a long command still posts", bool(await_posted_away(pane, since)), pane, since)
    time.sleep(STILL_UP_S)
    check(f"that banner is still up {STILL_UP_S} s later", bool(banners(pane)), pane, since)
    since = now_ms()
    run_in(pane, "true")
    check("after `omz reload`: next command clears it", bool(wait(lambda: not banners(pane), 8)), pane, since)


def check_claude_blocked(pane, run):
    need_terminal_behind()
    # The cheapest model will do, since only the permission prompt matters.
    run_in(pane, "claude --model haiku")

    def ready():
        info = pane_info(pane)
        return info.get("agent") == "claude" and info.get("agent_status") in ("idle", "done", "unknown")

    if not wait(ready, 30):
        check("Herdr detects Claude in the test pane", False, pane, 0)
        return
    time.sleep(3)
    need_terminal_behind()
    since = now_ms()
    run["banner_since"] = since
    run["claude"] = True
    save_run(run)
    herdr("pane", "send-text", pane, "Run this exact shell command with the Bash tool and nothing else: ls /tmp")
    time.sleep(0.5)
    herdr("pane", "send-keys", pane, "Enter")
    line = wait_for_log(pane, since, "Posted", 75)
    if not line and log_line(pane, since, "Watching"):
        raise Stop("the terminal came forward during this check, so it counted as watched")
    check("Claude asks permission, terminal behind: posts `Claude · blocked`", line and "Claude · blocked" in line, pane, since)
    if not line:
        print(herdr("pane", "read", pane, "--source", "visible", "--lines", "25").get("raw", "")[-1500:])


def shell_says(pane, line):
    run_in(pane, line)
    time.sleep(1.5)
    return herdr("pane", "read", pane, "--source", "recent", "--lines", "8").get("raw", "")


def start_run():
    plugin = herdr("plugin", "list").get("raw", "").strip().splitlines()
    print("plugin:", next((line.strip() for line in plugin if "herdr-nudge" in line), "not installed"))
    before = focused_panes()
    made = herdr("workspace", "create", "--no-focus", "--label", LABEL, "--cwd", REPO)["result"]
    root = made.get("root_pane") or made.get("pane")
    pane = root["pane_id"] if isinstance(root, dict) else root
    run = {"workspace": made["workspace"]["workspace_id"], "pane": pane, "panes": [pane], "focus_before": before}
    save_run(run)
    time.sleep(SHELL_START_S)
    return run


def wait_away():
    behind_since = None
    end = time.time() + CLICK_TIMEOUT_S
    while time.time() < end:
        if front_app() == TERMINAL:
            behind_since = None
        elif behind_since is None:
            behind_since = time.time()
        elif time.time() - behind_since >= SETTLED_AWAY_S:
            print("terminal is behind", front_app())
            return
        time.sleep(1)
    raise Stop(f"{TERMINAL} didn't stay behind another app within {CLICK_TIMEOUT_S} s")


def away():
    need_terminal_behind()
    run = start_run()
    pane = run["pane"]

    if "hook-loaded=1" not in shell_says(pane, 'print -r -- "hook-loaded=${+functions[_herdr_nudge_preexec]}"'):
        check("a new shell loads the zsh hook", False, pane, 0)
        return
    check("a new shell loads the zsh hook", True)
    # Moving focus would pull the view out from under someone who just came back.
    need_terminal_behind()
    focus(pane)
    time.sleep(1)

    check_failed_command(pane)
    check_next_command_clears(pane)
    check_omz_reload(pane)
    check_claude_blocked(pane, run)

    if not failures:
        print(f"\nBanner is up. Next: `await-click`, and ask for a click on `Claude · blocked` from `{LABEL}`.")
        print("Also ask: did that banner have the Claude logo and a sound? Did Herdr's sound play with `sleep · failed`?")


def click_only():
    # All banners click the same way, and staying away for all of `away` is a
    # lot to ask. Focus stays where it is, so this click has to move it too.
    need_terminal_behind()
    run = start_run()
    pane = run["pane"]
    since = now_ms()
    run_in(pane, SLEEP)
    check("long command in another pane: posts a banner", bool(wait_for_log(pane, since, "Posted", 20)), pane, since)
    run["banner_since"] = since
    save_run(run)


def await_click():
    run = load_run()
    if not run or "banner_since" not in run:
        raise Stop("no banner is waiting for a click; run `away` or `click-only` first")
    pane, since = run["pane"], run["banner_since"]
    last_front = {}

    def clicked():
        if jobs(pane):
            last_front["app"] = front_app()
            return False
        return True

    if not wait(clicked, CLICK_TIMEOUT_S, step=0.3):
        print(f"no click within {CLICK_TIMEOUT_S} s. The banner may still be up; run this again, or `clean`.")
        failures.append("no click")
        return
    # Whoever clicked may move on straight away, so any moment in this window
    # counts, not one sample at the end of it.
    raised = wait(lambda: front_app() == TERMINAL, 3, step=0.2)
    has_focus = wait(lambda: pane_info(pane).get("focused") is True, 3, step=0.2)
    # Coming back to the pane by hand, or answering the prompt, takes the
    # banner down too, and neither of those is a click.
    check("the banner went away from a click, not a withdrawal", not log_line(pane, since, "withdrew"), pane, since)
    check("click: the test pane has focus", bool(has_focus), pane, since)
    check("click: the banner is gone", not banners(pane), pane, since)
    print("\nNext: `front`, straight away, while the terminal is in front and nobody is typing.")
    # Someone asked to click often comes to the terminal to read that first.
    # A click then shows nothing about bringing the terminal forward.
    if last_front.get("app") == TERMINAL:
        raise Stop("the terminal was already in front at the click, so bringing it forward wasn't tested")
    check(f"click: the terminal comes forward from behind {last_front.get('app')}", bool(raised), pane, since)


def leave_claude(pane):
    # Enter on a live permission prompt would accept it, so /exit only goes in
    # once Escape has taken the prompt down.
    herdr("pane", "send-keys", pane, "Escape")
    if wait(lambda: pane_info(pane).get("agent_status") != "blocked", 6):
        herdr("pane", "send-text", pane, "/exit")
        time.sleep(0.5)
        herdr("pane", "send-keys", pane, "Enter")
        wait(lambda: not pane_info(pane).get("agent"), 8)


def check_watching(pane):
    need_terminal_in_front()
    focus(pane)
    time.sleep(1)
    since = now_ms()
    run_in(pane, SLEEP)
    time.sleep(SLEEP_S + 3)
    if front_app() != TERMINAL:
        raise Stop("the terminal went behind another app during this check")
    check("watched pane: outcome is Watching", bool(log_line(pane, since, "Watching")), pane, since)
    check("watched pane: no banner", not banners(pane) and not jobs(pane), pane, since)


def post_while_elsewhere(pane, other):
    """Starts a command, moves to the other pane before it ends, and returns
    the time it started and the Posted line."""
    need_terminal_in_front()
    since = now_ms()
    run_in(pane, SLEEP)
    time.sleep(SLEEP_S - 2)
    focus(other)
    return since, wait_for_log(pane, since, "Posted", 10)


def check_focus_back(pane, other):
    since, line = post_while_elsewhere(pane, other)
    check("moved to another pane: posts a banner", bool(line), pane, since)
    since = now_ms()
    focus(pane)
    check("moved back: banner goes away", bool(wait(lambda: not banners(pane), 6)), pane, since)
    check("moved back: outcome is Withdrawn", bool(wait_for_log(pane, since, "Withdrawn", 3)), pane, since)


def check_click_moves_focus(pane, other):
    # The real click in `await-click` lands on a pane that already has focus.
    # This one starts from another pane, and runs the click the way a banner
    # does: with almost no environment.
    since, line = post_while_elsewhere(pane, other)
    check("click from another pane: posts a banner", bool(line), pane, since)
    found = jobs(pane)
    if found:
        clean_env = {"HOME": os.environ["HOME"], "PATH": "/usr/bin:/bin"}
        subprocess.run([PLUGIN_BIN, "--click", found[0]], capture_output=True, text=True, env=clean_env, timeout=15)
        time.sleep(1)
    check("click from another pane: focus moves to its pane", focused_panes() == [pane], pane, since)
    check("click from another pane: the job is gone", not jobs(pane), pane, since)
    remove_banner(pane)


def front():
    # `away` leaves its workspace open for this. Nothing here needs its Claude
    # session, so a new workspace does just as well for a second try.
    run = load_run() or start_run()
    pane = run["pane"]
    if run.get("claude"):
        leave_claude(pane)
    second = herdr("pane", "split", pane, "--direction", "right", "--no-focus", "--cwd", REPO)["result"]["pane"]["pane_id"]
    run["panes"].append(second)
    save_run(run)
    time.sleep(SHELL_START_S)
    check_watching(second)
    check_focus_back(second, pane)
    check_click_moves_focus(second, pane)


def clean():
    run = load_run()
    if not run:
        print("nothing to clean")
        return
    panes = run["panes"]
    stayed = any(p in panes for p in focused_panes())
    herdr("workspace", "close", run["workspace"])
    time.sleep(1.5)
    for pane in panes:
        remove_banner(pane)
    # Someone who has moved elsewhere since the run started keeps their focus.
    if stayed and run["focus_before"]:
        focus(run["focus_before"][0])
    os.unlink(RUN_FILE)
    left = [(p, jobs(p)) for p in panes if jobs(p)]
    print(f"closed `{LABEL}`. Jobs left: {left or 'none'}. Focused: {focused_panes()}")


def main():
    stage = sys.argv[1] if len(sys.argv) == 2 else ""
    if stage not in ("wait-away", "away", "await-click", "click-only", "front", "clean"):
        print(__doc__)
        return 2
    if not os.environ.get("HERDR_SOCKET_PATH"):
        print("STOPPED: not inside a Herdr pane")
        return 3
    if stage in ("away", "click-only") and load_run():
        print(f"STOPPED: an earlier run is still open; run `{sys.argv[0]} clean` first")
        return 3
    try:
        if stage == "wait-away":
            wait_away()
        elif stage == "away":
            away()
            if failures:
                clean()
        elif stage == "await-click":
            await_click()
        elif stage == "click-only":
            click_only()
            if failures:
                clean()
        elif stage == "front":
            # Outside the try, so finding the terminal behind another app
            # leaves the run open for another attempt.
            need_terminal_in_front()
            try:
                front()
            finally:
                clean()
        else:
            clean()
    except Stop as stop:
        print(f"STOPPED: {stop}")
        if stage in ("away", "click-only"):
            clean()
        return 3
    if failures:
        print(f"\n{len(failures)} failed: {failures}")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
