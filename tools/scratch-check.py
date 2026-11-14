#!/usr/bin/env python3
"""Run the plugin on a throwaway Herdr and check what it does with real events.

    python3 tools/scratch-check.py             # plain zsh
    python3 tools/scratch-check.py --omz DIR   # with the oh-my-zsh in DIR

cargo test replays saved events and verify.py re-runs Herdr's queries, so
neither sees what a new Herdr's events do to the plugin. On Herdr 0.9.2 both
were happy while every shell banner vanished the moment it appeared.

This links this checkout as the plugin on a headless Herdr with a scratch
HOME, loads the zsh hook in its panes, and reads the outcomes from the
plugin's log and its jobs folder. A long command posts and the banner stays
up. Then the next command, moving to the pane, a click, and closing the
pane, its tab or its workspace each take one down. A stand-in `claude` puts
Claude Code's permission prompt on its screen for Herdr to read. Your own
server is never contacted.

The banners are real. On a Mac that allows them, a few show for a few
seconds: leave them alone, since a click takes one down before its check
gets to it. Nothing here raises a window. GitHub's Macs have notifications
turned off, so nothing shows there, and the checks read the same log.

Not covered: a watched pane (that needs a client attached), the physical
click, sounds, and real Claude Code.

Exit codes: 0 every check passed, 1 a check failed, 2 it stopped before a
check could fail. A 2 can still come from Herdr, when something the setup
needs stopped working; the message says what.
"""
import json
import os
import re
import shlex
import shutil
import socket
import subprocess
import sys
import time
import traceback

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
HERDR = os.environ.get("HERDR_BIN_PATH") or os.path.expanduser("~/.local/bin/herdr")
PLUGIN_BIN = os.path.join(REPO, "bin", "herdr-nudge")
NOTIFIER = os.path.join(REPO, "vendor", "HerdrNudge.app", "Contents", "MacOS", "terminal-notifier")
# Short: macOS caps a socket path at 104 bytes.
SCRATCH = "/tmp/herdr-nudge-scratch"
STATE = os.path.join(SCRATCH, ".local", "state", "herdr", "plugins", "herdr-nudge")
HOOK = os.path.join(STATE, "herdr-nudge.zsh")
STAND_IN = os.path.join(SCRATCH, "bin", "claude")
# .zshrc prints this last, followed by 1 if the hook loaded.
MARKER = "scratch-check: hook="
# Long enough to pass the hook's default min_seconds, which is 5.
LONG = "sleep 8; false"
POSTED = "sleep · failed"
# Herdr 0.9.2 took shell banners down within a second of posting them.
STILL_UP_S = 5

ENV = {
    "HOME": SCRATCH,
    "USER": os.environ.get("USER", ""),
    "PATH": "/usr/bin:/bin:/usr/sbin:/sbin:" + os.path.dirname(HERDR),
    "SHELL": "/bin/zsh",
    "LANG": "en_US.UTF-8",
}
# What macOS gives the click: no HERDR_* at all.
CLICK_ENV = {
    "HOME": SCRATCH,
    "USER": ENV["USER"],
    "PATH": "/usr/bin:/bin:/usr/sbin:/sbin",
    "SHELL": "/bin/zsh",
    "TMPDIR": os.environ.get("TMPDIR", "/tmp/"),
}

failures = []
# Panes whose banner group was empty when the checks started, so whatever is
# left in it at the end is ours to remove.
ours = []


class Unfinished(Exception):
    """Something the checks after this point need didn't happen."""


def fail_and_stop(name, why):
    """A check failed, and the ones after it can't run without it."""
    print(f"FAIL  {name}")
    failures.append(name)
    raise Unfinished(why)


# Herdr

def herdr(*argv, check=True):
    try:
        proc = subprocess.run([HERDR, *argv], capture_output=True, text=True, env=ENV, timeout=30)
    except subprocess.TimeoutExpired:
        raise Unfinished(f"herdr {' '.join(argv)}: no answer in 30 s")
    if check and proc.returncode != 0:
        raise Unfinished(f"herdr {' '.join(argv)}: exit {proc.returncode}: {proc.stderr.strip()}")
    return proc


def herdr_json(*argv):
    out = herdr(*argv).stdout
    try:
        return json.loads(out)["result"]
    except (ValueError, KeyError):
        raise Unfinished(f"herdr {' '.join(argv)}: no result in {out!r}")


def start_server():
    server = subprocess.Popen([HERDR, "server"], env=ENV, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    # A server that isn't up yet prints "status: not running", so match the
    # whole line.
    up = wait(lambda: re.search(r"^status: running$", herdr("status", "server", check=False).stdout, re.M), 10, 0.1)
    if not up:
        server.kill()
        raise Unfinished("scratch server did not start")
    return server


def stop_server(server):
    herdr("server", "stop", check=False)
    try:
        server.wait(10)
    except subprocess.TimeoutExpired:
        server.kill()
        server.wait()


def socket_path():
    found = re.search(r"^socket: (.+)$", herdr("status", "server").stdout, re.M)
    if not found:
        raise Unfinished("`herdr status server` names no socket")
    return found.group(1)


def focus(sock, pane):
    request = {"id": "scratch-check", "method": "pane.focus", "params": {"pane_id": pane}}
    with socket.socket(socket.AF_UNIX) as s:
        s.settimeout(5)
        s.connect(sock)
        s.sendall((json.dumps(request) + "\n").encode())
        reply = b""
        while not reply.endswith(b"\n"):
            chunk = s.recv(65536)
            if not chunk:
                break
            reply += chunk
    if b'"error"' in reply:
        raise Unfinished(f"pane.focus {pane}: {reply.decode().strip()}")


def workspace(label):
    made = herdr_json("workspace", "create", "--label", label, "--cwd", SCRATCH, "--no-focus")
    return made["workspace"]["workspace_id"], made["root_pane"]["pane_id"]


def split(pane):
    return herdr_json("pane", "split", pane, "--direction", "right", "--no-focus")["pane"]["pane_id"]


def run_in(pane, line):
    herdr("pane", "run", pane, line)


def shell_ready(pane, times=1):
    """Waits until the pane's zsh has been through .zshrc `times` times."""
    def seen():
        text = herdr("pane", "read", pane, "--source", "recent", "--lines", "60", check=False).stdout
        return text if text.count(MARKER) >= times else None

    text = wait(seen, 30)
    if not text:
        raise Unfinished(f"{pane}: zsh never got to the end of .zshrc")
    # The hook returns early, without hooking into the shell, when the pane
    # lacks Herdr's variables or shell.env switches it off. Either breaks
    # every shell banner.
    if MARKER + "1" not in text:
        fail_and_stop("the zsh hook hooks into a new pane's shell", "the shell checks need the hook")


# The plugin

def plugin_log(since):
    """Every hook run since `since`, oldest first, as (ms after, event, lines)."""
    proc = herdr("plugin", "log", "list", "--plugin", "herdr-nudge", "--limit", "200", check=False)
    # Every check reads this, so a change to its format would look like the
    # plugin doing nothing.
    # A run still going has no stderr yet.
    try:
        logs = json.loads(proc.stdout)["result"]["logs"]
        unreadable = [e for e in logs if "started_unix_ms" not in e
                      or ("stderr" not in e and e.get("status") != "running")]
    except (ValueError, KeyError, TypeError, AttributeError):
        raise Unfinished(f"can't read `herdr plugin log list`: {(proc.stdout or proc.stderr).strip()[:300]!r}")
    if unreadable:
        raise Unfinished(f"`herdr plugin log list` entry without started_unix_ms or stderr: {unreadable[0]!r}")
    picked = []
    for entry in logs:
        if entry["started_unix_ms"] >= since:
            lines = [line.replace("herdr-nudge: ", "") for line in entry.get("stderr", "").splitlines()]
            picked.append((entry["started_unix_ms"] - since, entry.get("event"), lines))
    return sorted(picked, key=lambda item: item[0])


def log_line(since, start, pane=None, contains=""):
    """The first line starting with `start` (and holding `contains`), from a
    run about `pane` if one is given."""
    for _, _, lines in plugin_log(since):
        if pane and not any(f"pane={pane} " in line for line in lines):
            continue
        for line in lines:
            if line.startswith(start) and contains in line:
                return line
    return None


def jobs(pane):
    found = []
    try:
        names = os.listdir(os.path.join(STATE, "jobs"))
    except OSError:
        return found
    for name in names:
        if not name.endswith(".json"):
            continue
        try:
            with open(os.path.join(STATE, "jobs", name)) as f:
                if json.load(f).get("pane_id") == pane:
                    found.append(name[: -len(".json")])
        except (OSError, ValueError):
            pass
    return found


def banners(pane):
    out = subprocess.run([NOTIFIER, "-list", f"herdr-nudge-{pane}"], capture_output=True, text=True, timeout=10)
    lines = [line for line in out.stdout.splitlines() if line.strip()]
    return lines[1:] if lines and lines[0].startswith("GroupID") else lines


def write_config(text):
    path = os.path.join(herdr("plugin", "config-dir", "herdr-nudge").stdout.strip(), "config.toml")
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as f:
        f.write(text)


def front_bundle():
    front = subprocess.run(["lsappinfo", "front"], capture_output=True, text=True).stdout.strip()
    info = subprocess.run(["lsappinfo", "info", "-only", "bundleid", front], capture_output=True, text=True).stdout
    # One line on macOS 26, an info block with a bundleID= line on 27.
    found = re.search(r'(?:"CFBundleIdentifier"|bundleID)="([^"]+)"', info)
    if not found:
        raise Unfinished(f"no bundle id for the app in front ({front!r})")
    return found.group(1)


# Checking

def wait(condition, timeout, step=0.5):
    deadline = time.monotonic() + timeout
    while True:
        found = condition()
        if found or time.monotonic() >= deadline:
            return found
        time.sleep(step)


def now_ms():
    return int(time.time() * 1000)


def check(name, ok, since, pane=None):
    print(f"{'ok  ' if ok else 'FAIL'}  {name}")
    if ok:
        return
    failures.append(name)
    if pane:
        print(f"      jobs: {jobs(pane)}  banners: {banners(pane)}")
    for ms, event, lines in plugin_log(since):
        if pane and not any(f"pane={pane} " in line for line in lines) and "closed" not in (event or ""):
            continue
        print(f"      +{ms} ms {event}")
        for line in lines:
            print(f"        {line}")


def write_zshrc(omz):
    lines = []
    if omz:
        lines += [
            f"export ZSH={shlex.quote(omz)}",
            f"export ZSH_CACHE_DIR={SCRATCH}/.cache/oh-my-zsh",
            "ZSH_THEME=robbyrussell",
            "plugins=(git)",
            "zstyle ':omz:update' mode disabled",
            # /tmp is world-writable, which compaudit would ask about.
            "ZSH_DISABLE_COMPFIX=true",
            'source "$ZSH/oh-my-zsh.sh"',
        ]
    # What `setup-zsh` appends.
    lines += [f"if [[ -r {HOOK} ]]; then", f"  source {HOOK}", "fi"]
    # 1 once the hook has added its preexec, which it does only after every
    # check that can switch it off. With no preexec at all the array is
    # unset, hence the :-0.
    lines.append(f'print -r -- "{MARKER}$(( ${{${{preexec_functions[(I)_herdr_nudge_preexec]}}:-0}} > 0 ))"')
    with open(os.path.join(SCRATCH, ".zshrc"), "w") as f:
        f.write("\n".join(lines) + "\n")


# Herdr takes a process called claude for the agent, and reads Claude's status
# off its screen with the rules in its agent manifests. A `herdr:claude` report
# binds a session but doesn't set the status (0.9.5). So this prints what
# Claude Code shows: `ask` a Bash permission prompt, `work` the line under a
# running turn. If Herdr's Claude rules stop matching these, check them against
# a real Claude before deciding the plugin broke.
STAND_IN_SCRIPT = r"""#!/bin/sh
printf '\033]0;claude\007'
while IFS= read -r line; do
  printf '\033[2J\033[H'
  case $line in
    ask) printf ' Bash command\n\n   ls /tmp\n\n Do you want to proceed?\n \342\235\257 1. Yes\n   2. No\n\n Esc to cancel\n' ;;
    work) printf '\342\234\273 Working\342\200\246 (3s \302\267 esc to interrupt)\n' ;;
  esac
done
"""


def write_stand_in():
    os.makedirs(os.path.dirname(STAND_IN))
    with open(STAND_IN, "w") as f:
        f.write(STAND_IN_SCRIPT)
    os.chmod(STAND_IN, 0o755)


def tell(pane, line):
    herdr("pane", "send-text", pane, line)
    herdr("pane", "send-keys", pane, "Enter")


def check_shell(pane, since, label=""):
    posted = wait(lambda: log_line(since, "Posted", pane, POSTED), 30)
    check(f"{label}long command, nobody watching: posts `{POSTED}`", bool(posted), since, pane)
    if not posted:
        return
    time.sleep(STILL_UP_S)
    gone = log_line(since, "withdrew ", pane) or log_line(since, "Withdrawn", pane)
    check(f"{label}{STILL_UP_S} s later: still up", not gone and bool(jobs(pane)), since, pane)
    since = now_ms()
    run_in(pane, "true")
    withdrew = wait(lambda: log_line(since, "withdrew ", pane), 10)
    check(f"{label}next command: taken down", bool(withdrew) and not jobs(pane), since, pane)


def run(omz):
    print(herdr("--version").stdout.strip(), "with", "oh-my-zsh" if omz else "plain zsh",
          "on a scratch server under", SCRATCH, "\n")
    write_zshrc(omz)
    write_stand_in()
    # Only a server start runs [[startup]], which writes the zsh hook, so the
    # plugin is linked first and the server restarted.
    server = start_server()
    try:
        herdr("plugin", "link", REPO)
    finally:
        stop_server(server)
    server = start_server()
    try:
        if not wait(lambda: os.path.exists(HOOK), 15):
            fail_and_stop("the plugin's startup writes the zsh hook", f"no {HOOK}")
        checks(socket_path(), omz)
    finally:
        stop_server(server)


def checks(sock, omz):
    _, shell = workspace("shell")
    _, click = workspace("click")
    click_other = split(click)
    _, focused = workspace("focus")
    focused_other = split(focused)
    _, closed = workspace("close-pane")
    split(closed)  # so closing the pane leaves the tab open
    tabs_ws, _ = workspace("close-tab")
    made = herdr_json("tab", "create", "--workspace", tabs_ws, "--cwd", SCRATCH, "--no-focus")
    tab, in_tab = made["tab"]["tab_id"], made["root_pane"]["pane_id"]
    gone_ws, in_ws = workspace("close-workspace")
    _, claude = workspace("claude")

    posting = [shell, click, focused, closed, in_tab, in_ws]
    for pane in posting + [claude]:
        shell_ready(pane)
        # The group has no server in it, so on a Mac with a real Herdr the
        # same pane id there may have a banner up already.
        if banners(pane):
            raise Unfinished(f"a banner for {pane} is up already, from another Herdr; not touching it")
        ours.append(pane)

    since = now_ms()
    for pane in posting:
        run_in(pane, LONG)
    check_shell(shell, since)
    for pane in posting[1:]:
        if not wait(lambda: log_line(since, "Posted", pane, POSTED), 20):
            fail_and_stop(f"long command in {pane}: posts `{POSTED}`", "the checks after it need its banner")

    # Run the way a banner runs it.
    focus(sock, click_other)
    found = jobs(click)
    since = now_ms()
    proc = subprocess.run([PLUGIN_BIN, "--click", found[0]] if found else ["true"],
                          capture_output=True, text=True, env=CLICK_ENV, timeout=15)
    on_click = wait(lambda: herdr_json("pane", "get", click)["pane"].get("focused"), 5)
    check("click: moves to its pane, job gone", bool(found) and bool(on_click) and not jobs(click), since, click)
    if not on_click:
        print(f"      click stderr: {proc.stderr.strip()}")

    since = now_ms()
    herdr("pane", "close", closed)
    taken = wait(lambda: log_line(since, "Withdrawn", closed), 10)
    check("pane closed: taken down", bool(taken) and not jobs(closed), since, closed)

    # Herdr sends no pane.closed for the panes of a closed tab or workspace,
    # so the tab.closed or workspace.closed hook takes the banner down
    # (PanesGone). If Herdr starts sending one, a Withdrawn from it is fine too.
    went = lambda pane: (log_line(since, "PanesGone", contains=f'"herdr-nudge-{pane}"')
                         or log_line(since, "Withdrawn", pane))
    since = now_ms()
    herdr("tab", "close", tab)
    taken = wait(lambda: went(in_tab), 10)
    check("tab closed: taken down", bool(taken) and not jobs(in_tab), since, in_tab)

    since = now_ms()
    herdr("workspace", "close", gone_ws)
    taken = wait(lambda: went(in_ws), 10)
    check("workspace closed: taken down", bool(taken) and not jobs(in_ws), since, in_ws)

    # Moving to a pane only counts as seeing it while its terminal is in
    # front. No client is attached to say which terminal that is, so the
    # app in front is named as the terminal here.
    write_config(f'default_terminal = "{front_bundle()}"\n')
    try:
        focus(sock, focused_other)
        time.sleep(0.5)
        since = now_ms()
        focus(sock, focused)
        taken = wait(lambda: log_line(since, "Withdrawn", focused), 10)
        check("moved to the pane: taken down", bool(taken) and not jobs(focused), since, focused)
    finally:
        write_config("")

    run_in(claude, STAND_IN)
    status = lambda: herdr_json("pane", "get", claude)["pane"]
    if not wait(lambda: status().get("agent") == "claude", 15):
        raise Unfinished(f"{claude}: Herdr didn't take the stand-in for Claude")
    tell(claude, "work")
    if not wait(lambda: status().get("agent_status") == "working", 15):
        raise Unfinished(f"{claude}: Herdr didn't read the stand-in's screen as working")
    since = now_ms()
    tell(claude, "ask")
    posted = wait(lambda: log_line(since, "Posted", claude, "blocked"), 15)
    check("Claude asks permission: posts `blocked`", bool(posted), since, claude)
    since = now_ms()
    tell(claude, "work")
    taken = wait(lambda: log_line(since, "withdrew ", claude), 10)
    check("Claude back to work: taken down", bool(taken) and not jobs(claude), since, claude)

    if omz:
        run_in(shell, "omz reload")
        shell_ready(shell, times=2)
        since = now_ms()
        run_in(shell, LONG)
        check_shell(shell, since, "after `omz reload`: ")


def main():
    args = sys.argv[1:]
    if args and (len(args) != 2 or args[0] != "--omz" or not os.path.isfile(os.path.join(args[1], "oh-my-zsh.sh"))):
        raise SystemExit(__doc__)
    omz = os.path.abspath(args[1]) if args else None
    if os.path.exists(SCRATCH):
        herdr("server", "stop", check=False)
        shutil.rmtree(SCRATCH)
    os.makedirs(SCRATCH)
    code = 0
    try:
        run(omz)
    except Unfinished as e:
        print(f"\nstopped: {e}")
        code = 2
    except (OSError, KeyError, subprocess.TimeoutExpired):
        traceback.print_exc()
        code = 2
    finally:
        try:
            herdr("server", "stop", check=False)
        except Unfinished:
            pass
        shutil.rmtree(SCRATCH, ignore_errors=True)
        # A failed check can leave its banner up, and with the scratch server
        # gone a click on it would do nothing.
        for pane in ours:
            try:
                subprocess.run([NOTIFIER, "-remove", f"herdr-nudge-{pane}"], capture_output=True, timeout=10)
            except subprocess.TimeoutExpired:
                pass
    # A check that failed before the run stopped still counts.
    if failures:
        print(f"\n{len(failures)} failed: " + "; ".join(failures))
        code = 1
    elif code == 0:
        print("\nall passed")
    sys.exit(code)


if __name__ == "__main__":
    main()
