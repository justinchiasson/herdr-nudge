#!/usr/bin/env python3
"""Promote probe captures into categorised test fixtures.

    python3 tools/fixtures/extract.py list  [LOG]   # index every record
    python3 tools/fixtures/extract.py write         # (re)write tests/fixtures/events
    python3 tools/fixtures/extract.py cli NAME ARGS # run `herdr ARGS`, save tests/fixtures/cli/NAME.json
    python3 tools/fixtures/extract.py scrub         # redact raw/, cli/, socket/ and sys/ in place
    python3 tools/fixtures/extract.py socket NAME METHOD PARAMS_JSON  # one socket request, saved to socket/NAME.json
    python3 tools/fixtures/extract.py sys NAME PROG ARGS  # run a macOS tool, save sys/NAME.json

Parses a probe log (tools/probe/dump.sh format) and writes one JSON file per
selected record. Nothing is hand-written: `event_json` and every env value are
copied byte-for-byte from the log. Records are selected by the 1-based line
number of their header line in a raw log, so SELECTIONS stays stable as long
as no raw log gains or loses a line. What Herdr wrote is never edited. Lines
written by tools/probe/mark.sh are kept as "#mark" records and attached to the
fixtures that follow them; those notes are ours and may be reworded.
"""
import getpass
import json
import os
import re
import socket
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
RAW_DIR = os.path.join(ROOT, "tests/fixtures/raw")
RAW = os.path.join(RAW_DIR, "events-2026-09-18.log")
OUT = os.path.join(ROOT, "tests/fixtures/events")
CLI = os.path.join(ROOT, "tests/fixtures/cli")
SOCKET = os.path.join(ROOT, "tests/fixtures/socket")
SYS = os.path.join(ROOT, "tests/fixtures/sys")
HERDR = os.environ.get("HERDR_BIN_PATH") or os.path.expanduser("~/.local/bin/herdr")

# These fixtures are published, so the capturing machine's identity comes out
# first. Substitutions are literal, applied everywhere, and idempotent: the
# shape of every path, title and id is preserved, only the values change.
# Session ids are mapped in order of first appearance, stably across files, so
# two fixtures referring to one session still agree. Deliberately NOT redacted:
# pane/workspace ids, timestamps, and terminal titles naming this project's own
# work — they are fixture content, and notifications are composed from them.
# Read from the machine at run time, so the names never appear in this file.
REDACTIONS = [
    (real, fake)
    for real, fake in [
        (getpass.getuser(), "dev"),
        (socket.gethostname().split(".")[0], "dev-mac"),
    ]
    if real and real != fake
]
UUID_RE = re.compile(r"\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b")
PLACEHOLDER = "00000000-0000-4000-8000-%012d"

# No mapping from real id to placeholder is ever written down — that file would
# be the leak. Within one run, one session keeps one placeholder; across runs,
# numbering continues past the highest placeholder already in the fixtures, so
# a later capture can't reuse an earlier session's number.


def redact(text, uuid_map, counter):
    for old, new in REDACTIONS:
        text = text.replace(old, new)

    def swap(match):
        real = match.group(0)
        if real.startswith("00000000-0000-4000-8000-"):
            return real  # already redacted; keeps this idempotent
        if real not in uuid_map:
            counter[0] += 1
            uuid_map[real] = PLACEHOLDER % counter[0]
        return uuid_map[real]

    return UUID_RE.sub(swap, text)


def scrub():
    """Redact raw/, cli/, socket/ and sys/ in place. Run `write` afterwards
    to regenerate events/ from the redacted logs."""
    targets = [os.path.join(RAW_DIR, n) for n in sorted(os.listdir(RAW_DIR)) if n.endswith(".log")]
    for d in (CLI, SOCKET, SYS):
        if os.path.isdir(d):
            targets += [os.path.join(d, n) for n in sorted(os.listdir(d)) if n.endswith(".json")]
    contents = {}
    used = [0]
    for path in targets:
        with open(path, encoding="utf-8") as f:
            contents[path] = f.read()
        for found in UUID_RE.findall(contents[path]):
            if found.startswith("00000000-0000-4000-8000-"):
                used[0] = max(used[0], int(found.rsplit("-", 1)[1]))
    uuid_map, changed = {}, 0
    for path, before in contents.items():
        after = redact(before, uuid_map, used)
        if after != before:
            with open(path, "w", encoding="utf-8") as f:
                f.write(after)
            print("redacted", os.path.relpath(path, ROOT))
            changed += 1
    print(f"{changed} file(s) changed, {len(uuid_map)} session id(s) replaced")

# Provenance: "manual" = a person or a real agent did it (typing, clicking, an
# agent's own hooks); "programmatic" = driven by the herdr CLI or socket;
# "unknown" = the log doesn't say. Manual and programmatic have been seen to
# behave differently, so never let one stand in for the other.
#
# The Herdr that wrote each raw log. Behaviour has changed between versions
# (0.9.0 sent no focus events for manual navigation, 0.9.1 does), so every
# fixture says which one it came from.
HERDR_VERSIONS = {
    "events-2026-09-18.log": "0.9.0",
    "events-2026-09-18-gaps.log": "0.9.0",
    "events-2026-09-20-socket-focus.log": "0.9.0",
    "events-2026-09-23-herdr-0.9.1.log": "0.9.1",
    "events-2026-09-26-herdr-0.9.1-closes.log": "0.9.1",
    "events-2026-09-26-herdr-0.9.0-closes.log": "0.9.0",
    "events-2026-09-29-herdr-0.9.2.log": "0.9.2",
    "events-2026-09-29-herdr-0.9.2-clear.log": "0.9.2",
}

# {raw log: [(category, name, header line, provenance, why)]}
SELECTIONS = {
    "events-2026-09-18.log": [
        ("agent", "blocked", 118, "manual", "Claude asks for permission: an agent blocked"),
        ("agent", "done", 430, "manual", "Claude finished while the user was away: the unwatched completion"),
        ("agent", "idle-watched-completion", 92, "manual", "Claude finished while watched: Herdr reports idle, not done"),
        ("agent", "working", 105, "manual", "Claude working: never notifies"),
        ("agent", "idle-first-after-claim", 832, "manual", "First status after Claude claimed a pane a shell reporter had released"),
        ("shell", "idle-with-title-labels", 1, "programmatic", "Shell report with title, display_agent and state_labels, watched"),
        ("shell", "blocked-with-title-labels", 14, "programmatic", "Shell reporter sending blocked, all optional fields present"),
        ("shell", "blocked-bare", 27, "programmatic", "Shell report with no title, display_agent or state_labels (fields missing, not null)"),
        ("shell", "idle-without-working", 327, "programmatic", "Shell idle with no preceding working stays idle"),
        ("shell", "status-unknown-on-release", 40, "programmatic", "agent_status \"unknown\" sent when the reporter releases"),
        ("detected", "shell-claim", 326, "programmatic", "Shell reporter claims a pane: no released/final_status fields"),
        ("detected", "shell-release", 41, "programmatic", "Shell reporter releases: released=true, final_status"),
        ("detected", "agent-claim-after-shell-release", 819, "manual", "Claude detected in a pane a shell reporter used earlier"),
        ("lifecycle", "pane-closed", 807, "manual", "Agent pane closed; no agent_detected release precedes it"),
        ("lifecycle", "pane-created", 1222, "unknown", "New plain pane: nested pane object, not flat fields"),
    ],
    "events-2026-09-18-gaps.log": [
        ("agent", "blocked-user-elsewhere", 939, "manual", "Claude blocked 24 s after the user moved to another pane; focused_pane_id is still the event's pane"),
        ("agent", "done-user-elsewhere", 833, "manual", "Claude done ~20 s after the user moved away; focused_pane_id is still the event's pane"),
        ("agent", "status-unknown-no-agent-field", 1071, "manual", "Status event after Claude /exit: agent field missing entirely"),
        ("detected", "agent-release-on-exit", 1058, "manual", "Claude /exit: released=true, final_status=idle"),
        ("detected", "shell-claim-after-agent-exit", 1232, "programmatic", "Shell reporter claims the pane Claude just left: agent -> shell handover"),
        ("shell", "done-unwatched-failed", 94, "programmatic", "Reported idle on an unwatched pane arrives as done; state_labels idle=failed kept"),
        ("shell", "done-unwatched-after-handover", 1258, "programmatic", "Reported idle arrives as done after working, in the handover pane"),
        ("shell", "working-metadata-update", 81, "programmatic", "report-metadata alone emits a status event with an unchanged status, so the same status arrives twice"),
        ("shell", "blocked-unfocused-pane", 203, "programmatic", "Blocked on a pane that is not its tab's focused pane; focused_pane_id is the event's pane"),
        ("lifecycle", "tab-created", 2, "programmatic", "herdr tab create --no-focus"),
        ("focus", "tab-focus-tab-focused", 1552, "programmatic", "herdr tab focus w3:t2: burst of three in one second"),
        ("focus", "tab-focus-pane-focused", 1553, "programmatic", "Same burst: pane.focused for the tab's focused pane"),
        ("focus", "tab-focus-workspace-focused", 1554, "programmatic", "Same burst: workspace.focused fires even within one workspace; no pane_id"),
        ("focus", "tab-focus-back-pane-focused", 1606, "programmatic", "herdr tab focus w3:t1: second burst, different event order"),
    ],
    "events-2026-09-20-socket-focus.log": [
        ("focus", "socket-pane-focus-pane-focused", 4, "programmatic", "Socket pane.focus, the call a click makes, emits pane.focused for the pane"),
        ("focus", "socket-pane-focus-tab-focused", 2, "programmatic", "Same burst: tab.focused, even though the tab did not change"),
        ("focus", "socket-pane-focus-workspace-focused", 3, "programmatic", "Same burst: workspace.focused, even though the workspace did not change"),
    ],
    "events-2026-09-23-herdr-0.9.1.log": [
        ("focus", "manual-tab-click-pane-focused", 18, "manual", "User clicked another tab: pane.focused for its pane. 0.9.0 sent nothing for this"),
        ("focus", "manual-tab-click-tab-focused", 17, "manual", "Same click: tab.focused"),
        ("focus", "manual-tab-click-workspace-focused", 16, "manual", "Same click: workspace.focused, though the workspace did not change"),
        ("focus", "manual-pane-click-pane-focused", 174, "manual", "User clicked the other pane in the same tab: pane.focused"),
        ("focus", "manual-workspace-click-pane-focused", 250, "manual", "User clicked another workspace: pane.focused for its focused pane"),
        ("focus", "socket-pane-focus-moved-pane-focused", 569, "programmatic", "Socket pane.focus onto a pane in another tab, the call a click makes"),
        ("shell", "done-focused-terminal-in-background", 448, "programmatic", "Reported idle arrives as done on a pane focused in Herdr while the terminal app is in the background"),
        ("lifecycle", "pane-closed-by-cli", 555, "programmatic", "herdr pane close on a pane in a background tab"),
    ],
    "events-2026-09-26-herdr-0.9.1-closes.log": [
        ("lifecycle", "tab-closed", 576, "manual", "User closed a tab with two panes: tab.closed only, no pane.closed, and no pane ids"),
        ("lifecycle", "workspace-closed", 668, "manual", "User closed a workspace with two tabs: workspace.closed only, no tab.closed or pane.closed"),
        ("lifecycle", "tab-closed-by-cli", 41, "programmatic", "herdr tab close on a background tab with two panes"),
        ("lifecycle", "workspace-closed-by-cli", 133, "programmatic", "herdr workspace close on a background workspace with two tabs and three panes"),
        ("lifecycle", "tab-closed-by-pane-move", 797, "programmatic", "herdr pane move took the tab's only pane into another tab: tab.closed while the pane lives on"),
    ],
    "events-2026-09-26-herdr-0.9.0-closes.log": [
        ("lifecycle", "tab-closed-by-cli-0.9.0", 119, "programmatic", "herdr tab close on a background tab with two panes"),
        ("lifecycle", "workspace-closed-by-cli-0.9.0", 211, "programmatic", "herdr workspace close on a background workspace with two tabs and three panes"),
        ("lifecycle", "tab-closed-by-pane-move-0.9.0", 381, "programmatic", "herdr pane move took the tab's last pane to another workspace: tab.closed, and the pane's id changed"),
    ],
    "events-2026-09-29-herdr-0.9.2.log": [
        ("shell", "done-unwatched-failed-0.9.2", 419, "manual", "zsh hook, a failing command while the user was in another pane: done with idle=failed"),
        ("shell", "released-by-herdr-after-done-0.9.2", 432, "manual", "Herdr 0.9.2 releases a reported shell command itself once the prompt is back, in the same second as the done: unknown, still naming the agent"),
    ],
    "events-2026-09-29-herdr-0.9.2-clear.log": [
        ("shell", "cleared-on-next-command-0.9.2", 185, "programmatic", "The zsh hook clears the title and labels when the next command starts; on a pane nobody claims, Herdr sends unknown with no agent"),
    ],
}


def parse(path):
    """Return records in log order: header fields plus the env block that
    dump.sh wrote for it. Events fired in the same instant write their header
    lines first and their env blocks afterwards in any order, so each env block
    is matched to the oldest pending header with the same HERDR_PLUGIN_EVENT."""
    records, pending, block = [], [], None

    def flush():
        if block is None:
            return
        name = block["env"].get("HERDR_PLUGIN_EVENT")
        for i, rec in enumerate(pending):
            if rec["event"] == name:
                rec["env"] = block["env"]
                pending.pop(i)
                return
        raise SystemExit(f"{path}:{block['line']}: env block for {name!r} has no header")

    with open(path, encoding="utf-8") as f:
        for lineno, raw in enumerate(f, 1):
            line = raw.rstrip("\n")
            if line.startswith("    "):
                key, sep, value = line[4:].partition("=")
                if not sep:
                    raise SystemExit(f"{path}:{lineno}: malformed env line")
                if block is None or key in block["env"]:
                    flush()
                    block = {"line": lineno, "env": {}}
                block["env"][key] = value
                continue
            flush()
            block = None
            time, event, event_json = line.split("\t", 2)
            rec = {"line": lineno, "time": time, "event": event, "event_json": event_json}
            records.append(rec)
            if event != "#mark":  # tools/probe/mark.sh: no env block follows
                pending.append(rec)
    flush()
    if pending:
        raise SystemExit(f"{path}: headers without env: {[r['line'] for r in pending]}")
    return records


def summary(rec):
    if rec["event"] == "#mark":
        return f"{rec['line']:>5} {rec['time']} ---- {rec['event_json']}"
    data = json.loads(rec["event_json"])["data"]
    ctx = json.loads(rec["env"].get("HERDR_PLUGIN_CONTEXT_JSON", "{}"))
    extra = {k: v for k, v in data.items() if k not in ("type", "workspace_id")}
    return (f"{rec['line']:>5} {rec['time']} {rec['event']:<26} {json.dumps(extra)[:110]}"
            f"  | focused={ctx.get('focused_pane_id')}")


def main():
    cmd = sys.argv[1] if len(sys.argv) > 1 else "list"
    if cmd == "list":
        for rec in parse(sys.argv[2] if len(sys.argv) > 2 else RAW):
            print(summary(rec))
    elif cmd == "write":
        write()
    elif cmd == "cli" and len(sys.argv) > 3:
        cli(sys.argv[2], sys.argv[3:])
    elif cmd == "socket" and len(sys.argv) == 5:
        socket_request(sys.argv[2], sys.argv[3], json.loads(sys.argv[4]))
    elif cmd == "sys" and len(sys.argv) > 3:
        sys_command(sys.argv[2], sys.argv[3:])
    elif cmd == "scrub":
        scrub()
    else:
        raise SystemExit(__doc__)


def cli(name, argv):
    """Run a read-only herdr query and store its exit code and stdout verbatim."""
    import datetime
    import subprocess

    version = subprocess.run([HERDR, "--version"], capture_output=True, text=True).stdout.strip()
    proc = subprocess.run([HERDR] + argv, capture_output=True, text=True)
    fixture = {
        "argv": ["herdr"] + argv,
        "captured_at": datetime.datetime.now().isoformat(timespec="seconds"),
        "herdr_version": version,
        "exit_code": proc.returncode,
        "stdout": proc.stdout,
        "stderr": proc.stderr,
    }
    path = os.path.join(ROOT, "tests/fixtures/cli", name + ".json")
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w", encoding="utf-8") as f:
        json.dump(fixture, f, indent=2, ensure_ascii=False)
        f.write("\n")
    scrub()  # a live capture carries the machine's identity; never leave it unredacted
    print(os.path.relpath(path, ROOT), "exit", proc.returncode)


def write():
    import shutil

    shutil.rmtree(OUT, ignore_errors=True)  # events/ is fully generated
    index = []
    for log, selections in SELECTIONS.items():
        raw = os.path.join(RAW_DIR, log)
        rel_raw = os.path.relpath(raw, ROOT)
        records = parse(raw)
        by_line = {r["line"]: r for r in records}
        for category, name, line, provenance, why in selections:
            rec = by_line.get(line)
            if rec is None or rec["event"] == "#mark":
                raise SystemExit(f"no event header at {rel_raw}:{line}")
            marks = [r for r in records if r["event"] == "#mark" and r["line"] < line]
            fixture = {
                "source": f"{rel_raw}:{line}",
                "herdr_version": HERDR_VERSIONS[log],
                "captured_at": rec["time"],
                "provenance": provenance,
                "mark": marks[-1]["event_json"] if marks else None,
                "why": why,
                "event": rec["event"],
                "event_json": rec["event_json"],
                "env": rec["env"],
            }
            path = os.path.join(OUT, category, name + ".json")
            if os.path.exists(path):
                raise SystemExit(f"duplicate fixture name {category}/{name}")
            os.makedirs(os.path.dirname(path), exist_ok=True)
            with open(path, "w", encoding="utf-8") as f:
                json.dump(fixture, f, indent=2, ensure_ascii=False)
                f.write("\n")
            index.append(os.path.relpath(path, ROOT))
    print("\n".join(index))




def save(directory, name, fixture):
    path = os.path.join(directory, name + ".json")
    os.makedirs(directory, exist_ok=True)
    with open(path, "w", encoding="utf-8") as f:
        json.dump(fixture, f, indent=2, ensure_ascii=False)
        f.write("\n")
    scrub()
    return path


def socket_request(name, method, params):
    """Send one request to the Herdr socket and store the raw reply line.

    Only use methods that are safe to repeat: this talks to the live server."""
    import datetime
    import subprocess

    version = subprocess.run([HERDR, "--version"], capture_output=True, text=True).stdout.strip()
    path = os.environ.get("HERDR_SOCKET_PATH") or os.path.expanduser("~/.config/herdr/herdr.sock")
    request, reply = ask_socket(path, method, params)
    out = save(SOCKET, name, {
        "captured_at": datetime.datetime.now().isoformat(timespec="seconds"),
        "herdr_version": version,
        "request": request,
        "response": reply,
    })
    print(os.path.relpath(out, ROOT))


def ask_socket(path, method, params, timeout=2):
    """One request line to a Herdr socket. Returns (request, reply line)."""
    import socket

    request = json.dumps({"id": "capture", "method": method, "params": params}) + "\n"
    client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    client.settimeout(timeout)
    client.connect(path)
    client.sendall(request.encode())
    reply = b""
    while not reply.endswith(b"\n"):
        chunk = client.recv(65536)
        if not chunk:
            break
        reply += chunk
    client.close()
    return request, reply.decode()


def sys_command(name, argv):
    """Run a macOS tool (lsappinfo) and store its exit code and output."""
    import datetime
    import platform
    import subprocess

    proc = subprocess.run(argv, capture_output=True, text=True)
    # The bare program name, like cli() does: the test runner matches a
    # recording by file name, because the code calls tools by absolute path.
    out = save(SYS, name, {
        "argv": [os.path.basename(argv[0])] + argv[1:],
        "captured_at": datetime.datetime.now().isoformat(timespec="seconds"),
        "macos_version": platform.mac_ver()[0],
        "exit_code": proc.returncode,
        "stdout": proc.stdout,
        "stderr": proc.stderr,
    })
    print(os.path.relpath(out, ROOT), "exit", proc.returncode)


if __name__ == "__main__":
    main()
