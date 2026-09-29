#!/usr/bin/env python3
"""Run the cli/, socket/ and sys/ captures again and print what moved.

    python3 tools/fixtures/verify.py            # compare; exit 1 if anything moved
    python3 tools/fixtures/verify.py --record   # also save the installed Herdr's
                                                # output where it moved

The tests replay these captures byte for byte, so they stay green whatever a
new Herdr does. Run this after upgrading Herdr.

It compares shapes, not values: for JSON, every key path and the type found
there; for text, each line with numbers, hex, quoted names, variable values
and home directories masked. Values are compared only for exit codes and the
keys in STABLE, outside arrays. Everything else (ids, titles, cwd, revision, timestamps,
statuses) depends on the state it was captured in, which is rebuilt here
rather than repeated.

Herdr runs as a throwaway headless server under a scratch HOME, built up to
the state each capture needs, and deleted afterwards. Your own server is never
contacted. Each Herdr capture is compared with the installed version's, or
with the newest older one if there isn't one. --record saves a new version's
capture only where something moved: an older capture that still matches
already describes the new version, so a copy would only add files. sys/
captures record macOS tools, not Herdr, and are never re-recorded here.
"""
import datetime
import fcntl
import json
import os
import pty
import re
import shutil
import signal
import struct
import subprocess
import sys
import termios
import threading
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import extract  # noqa: E402

ROOT = extract.ROOT
FIXTURES = os.path.join(ROOT, "tests/fixtures")
HERDR = extract.HERDR
# Short and fixed: macOS caps a socket path at 104 bytes, and a recorded
# capture should name the same scratch paths every time it is re-recorded.
SCRATCH = "/tmp/herdr-nudge-verify"
# The session the sys/ captures were made under; it shows in socket paths and
# a client's argv, so matching it keeps those lines comparable.
SESSION = "nudge-capture"
PGREP = ["pgrep", "-a", "-lf", "^-?([^ ]*/)?herdr( |$)"]
# The only values compared: reply and error kinds, and agent labels. The rest
# is state that differs between any two captures. Not inside arrays: a list's
# members are data, and agent-manifests' agents come from a remote registry
# that changes without Herdr changing.
STABLE = {"type", "code", "id", "kind", "agent"}
# Placeholder-shaped, so extract.scrub leaves it alone.
SESSION_ID = "00000000-0000-4000-8000-000000000000"

# Captures that repeat another one's command with different processes or apps
# around, so they add no format to check, or that can't run unattended.
SKIPPED = {
    "sys/lsappinfo-bundleid-ghostty-macos27": "same command as lsappinfo-bundleid-ghostty, as macOS 27 prints it",
    "sys/lsappinfo-bundleid-textedit-macos27": "same command as lsappinfo-bundleid-ghostty, as macOS 27 prints it",
    "sys/lsappinfo-bundleid-gone-macos27": "same command as lsappinfo-bundleid-gone, as macOS 27 prints it",
    "sys/lsappinfo-bundleid-quit-macos27": "same command as lsappinfo-bundleid-gone, for an app that quit, on macOS 27",
    "sys/lsappinfo-front-macos27": "same command as lsappinfo-front, as macOS 27 prints it",
    "sys/lsappinfo-find-iterm": "same output as lsappinfo-find-ghostty, and needs iTerm running",
    "sys/open-bundle-ghostty": "would bring a terminal to the front",
    "sys/lsof-capture-session-clients": "same command as lsof-capture-session-one-client, more processes",
    "sys/ps-env-capture-session-clients": "same command as ps-env-capture-session-one-client, more processes",
    "sys/pgrep-herdr-three-servers": "same command as pgrep-herdr-two-sessions",
    "sys/pgrep-herdr-documents-servers": "same command as pgrep-herdr-two-sessions",
    "sys/ps-env-server-xdg": "same command as ps-env-server-plain, one more variable",
    "sys/ps-env-server-documents": "same command as ps-env-server-plain",
    "sys/ps-env-server-no-home": "same command as ps-env-server-plain, one variable fewer",
}


def version_of(text):
    return tuple(int(n) for n in re.findall(r"\d+", text)[:3])


class Verify:
    def __init__(self, record):
        self.record = record
        self.version = subprocess.run([HERDR, "--version"], capture_output=True, text=True).stdout.strip()
        self.drifted = 0
        self.checked = set()
        self.recorded = []
        self.children = []
        home = extract.redact(os.path.expanduser("~"), {}, [0])
        self.homes = sorted({home, "/Users/dev", "/private" + SCRATCH, SCRATCH}, key=len, reverse=True)

    # Comparing

    def shape(self, text):
        try:
            value = json.loads(text)
        except ValueError:
            return self.text_shape(text)
        out = set()
        walk(value, "", out)
        return out

    def text_shape(self, text):
        text = extract.redact(text, {}, [0])
        for home in self.homes:
            text = text.replace(home, "~")
        text = re.sub(r"0x[0-9a-fA-F]+", "0x#", text)
        text = re.sub(r"\d+", "#", text)
        text = re.sub(r'([=-])"[^"]*"', r'\1"…"', text)
        text = re.sub(r"\b([A-Za-z_]\w*)=\S*", r"\1=…", text)
        lines = set()
        for line in text.splitlines():
            words = []
            for word in line.split():
                if not words or words[-1] != word:  # visibleProcessList: one entry per app
                    words.append(word)
            if words:
                lines.add(" ".join(words))
        return lines

    def fixture(self, kind, base):
        """The capture to compare with: the installed version's, else the
        newest older one, else (Herdr downgraded) the oldest newer one.
        Returns (path, "same" | "older" | "newer")."""
        directory = os.path.join(FIXTURES, kind)
        found = []
        for name in os.listdir(directory):
            stem = name[:-5] if name.endswith(".json") else None
            if stem and re.sub(r"-\d+\.\d+\.\d+$", "", stem) == base:
                path = os.path.join(directory, name)
                with open(path, encoding="utf-8") as f:
                    found.append((version_of(json.load(f).get("herdr_version", "")), path))
        if not found:
            raise SystemExit(f"no capture named {kind}/{base}")
        mine = version_of(self.version)
        exact = [p for v, p in found if v == mine]
        if exact:
            return exact[0], "same"
        older = sorted(f for f in found if f[0] < mine)
        if older:
            return older[-1][1], "older"
        return min(found)[1], "newer"

    def matches(self, kind, base, stdout):
        with open(self.fixture(kind, base)[0], encoding="utf-8") as f:
            return self.shape(json.load(f)["stdout"]) == self.shape(stdout)

    def compare(self, kind, base, live, fields, save, mask=None):
        """`mask`, if given, is applied to both sides' stdout first."""
        label = f"{kind}/{base}"
        self.checked.add(label)
        if kind == "sys":
            path, relation = os.path.join(FIXTURES, kind, base + ".json"), "same"
        else:
            path, relation = self.fixture(kind, base)
        with open(path, encoding="utf-8") as f:
            old = json.load(f)
        moved = []
        for field in fields:
            if field == "exit_code":
                if old[field] != live[field]:
                    moved.append(f"  exit code {old[field]} -> {live[field]}")
                continue
            before, after = old[field], live[field]
            if mask and field == "stdout":
                before, after = mask(before), mask(after)
            before, after = self.shape(before), self.shape(after)
            moved += [f"  {field} - {s}" for s in sorted(before - after)]
            moved += [f"  {field} + {s}" for s in sorted(after - before)]
        against = ""
        if relation != "same":
            against = f" (vs {os.path.relpath(path, FIXTURES)}, {old.get('herdr_version')}, {relation})"
        if moved:
            self.drifted += 1
            print(f"MOVED {label}{against}")
            print("\n".join(moved))
        else:
            print(f"ok    {label}{against}")
        # Never for a downgrade: a version's own capture marks where it
        # differs from the one before it.
        if moved and relation == "older" and self.record:
            name = f"{base}-{'.'.join(map(str, version_of(self.version)))}"
            extract.save(os.path.join(FIXTURES, kind), name, save)
            self.recorded.append(f"{kind}/{name}")

    # Running

    def herdr(self, *argv, check=True, env=None):
        proc = subprocess.run([HERDR, *argv], capture_output=True, text=True, env=env or SERVER_ENV)
        if check and proc.returncode != 0:
            raise SystemExit(f"herdr {' '.join(argv)}: exit {proc.returncode}: {proc.stderr.strip()}")
        return proc

    def cli(self, base, *argv, settle=0, env=None):
        """With `settle`, keep asking for up to that many seconds while the
        reply doesn't match yet, for output that fills in after startup."""
        deadline = time.monotonic() + settle
        while True:
            proc = self.herdr(*argv, check=False, env=env)
            if time.monotonic() >= deadline or self.matches("cli", base, proc.stdout):
                break
            time.sleep(0.2)
        live = {"exit_code": proc.returncode, "stdout": proc.stdout, "stderr": proc.stderr}
        save = {
            "argv": ["herdr", *argv],
            "captured_at": now(),
            "herdr_version": self.version,
            **live,
        }
        self.compare("cli", base, live, ["exit_code", "stdout", "stderr"], save)

    def socket_call(self, base, method, params):
        request, reply = extract.ask_socket(self.socket_path, method, params, timeout=5)
        live = {"request": request, "response": reply}
        save = {"captured_at": now(), "herdr_version": self.version, **live}
        self.compare("socket", base, live, ["response"], save)

    def tool(self, base, argv, keep=None, mask=None):
        """`keep`: only stdout lines starting with one of these pids."""
        proc = subprocess.run(argv, capture_output=True, text=True)
        stdout = proc.stdout
        if keep is not None:
            stdout = "".join(l for l in stdout.splitlines(True) if l.split(" ", 1)[0] in keep)
        live = {"exit_code": proc.returncode, "stdout": stdout, "stderr": proc.stderr}
        self.compare("sys", base, live, ["exit_code", "stdout", "stderr"], None, mask)
        return stdout

    def pane(self, pane_id):
        return json.loads(self.herdr("pane", "get", pane_id).stdout)["result"]["pane"]

    def wait_for(self, pane_id, key, what):
        for _ in range(100):
            if key in self.pane(pane_id):
                return
            time.sleep(0.1)
        raise SystemExit(f"{pane_id}: no {key} after 10 s ({what})")

    def titled(self, pane_id):
        # Real panes get a title from the shell prompt; a scratch shell sets none.
        self.herdr("pane", "run", pane_id, "printf '\\033]0;verify\\007'")
        self.wait_for(pane_id, "terminal_title", "title escape")

    def split(self, pane_id):
        out = self.herdr("pane", "split", pane_id, "--direction", "right", "--no-focus")
        return json.loads(out.stdout)["result"]["pane"]["pane_id"]

    def run(self):
        print(f"{self.version}, scratch server under {SCRATCH}\n")
        server = subprocess.Popen([HERDR, "server"], env=SERVER_ENV,
                                  stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self.children.append(server.pid)
        for _ in range(100):
            status = self.herdr("status", "server", check=False).stdout
            found = re.search(r"^socket: (.+)$", status, re.M)
            # A server that isn't up yet prints "status: not running" and
            # still names its socket.
            if re.search(r"^status: running$", status, re.M) and found:
                self.socket_path = found.group(1)
                break
            time.sleep(0.1)
        else:
            raise SystemExit("scratch server did not start")

        # A focused claude pane with a session, and a plain shell beside it.
        created = json.loads(self.herdr("workspace", "create", "--label", "herdr-nudge", "--cwd", SCRATCH).stdout)
        ws = created["result"]["workspace"]["workspace_id"]
        claude = created["result"]["root_pane"]["pane_id"]
        shell = self.split(claude)
        self.herdr("pane", "run", claude, os.path.join(SCRATCH, "bin", "claude"))
        self.wait_for(claude, "agent", "stand-in claude")
        self.herdr("pane", "report-agent", claude, "--source", "herdr:claude", "--agent", "claude",
                   "--state", "working", "--agent-session-id", SESSION_ID, "--seq", "1")
        self.wait_for(claude, "agent_session", "session report")
        self.wait_for(claude, "terminal_title", "stand-in claude's title")
        self.titled(shell)

        self.cli("pane-list", "pane", "list")
        self.cli("pane-get-focused", "pane", "get", claude)
        self.cli("agent-get-with-session", "agent", "get", claude)
        self.cli("agent-get-plain-shell", "agent", "get", shell)
        self.cli("workspace-get", "workspace", "get", ws)
        self.cli("workspace-get-not-found", "workspace", "get", "w99")
        self.cli("plugin-config-dir", "plugin", "config-dir", "herdr-nudge")
        # Asked the way a hook asks it: by socket, with no session name, so
        # `session` comes back null as in the capture.
        by_socket = {k: v for k, v in SERVER_ENV.items() if k != "HERDR_SESSION"}
        self.cli("status-server", "status", "server", "--json",
                 env={**by_socket, "HERDR_SOCKET_PATH": self.socket_path})
        self.socket_call("pane-focus-ok", "pane.focus", {"pane_id": claude})  # already focused
        self.socket_call("pane-focus-not-found", "pane.focus", {"pane_id": "w999:p1"})
        # No client is attached yet, so Herdr shows and plays nothing, and
        # with toasts off by default it answers `disabled`.
        self.socket_call("notification-show", "notification.show",
                         {"title": "sleep 8 · done", "body": "herdr-nudge", "sound": "done"})

        # A shell command claimed the way our zsh hook does, then released.
        self.herdr("pane", "report-agent", shell, "--source", "verify", "--agent", "make",
                   "--state", "working", "--seq", "1")
        self.herdr("pane", "report-metadata", shell, "--source", "verify", "--title",
                   "make test · exit 0 · 2m11s", "--state-label", "idle=finished", "--seq", "2")
        self.cli("pane-get-reported-no-session", "pane", "get", shell)
        self.cli("pane-get-unfocused", "pane", "get", shell)
        failed = self.split(shell)
        self.titled(failed)
        self.herdr("pane", "report-agent", failed, "--source", "verify", "--agent", "make",
                   "--state", "working", "--seq", "1")
        self.herdr("pane", "report-metadata", failed, "--source", "verify", "--title",
                   "make test · exit 2 · 1m04s", "--display-agent", "make",
                   "--state-label", "idle=failed", "--seq", "2")
        self.herdr("pane", "report-agent", failed, "--source", "verify", "--agent", "make",
                   "--state", "idle", "--seq", "3")
        self.cli("agent-get-reported-no-session", "agent", "get", failed)
        self.herdr("pane", "release-agent", shell, "--source", "verify", "--agent", "make", "--seq", "3")
        self.cli("pane-get-after-release", "pane", "get", shell)
        unlabelled = json.loads(self.herdr("workspace", "create", "--label", "", "--no-focus").stdout)
        self.cli("workspace-get-empty-label", "workspace", "get",
                 unlabelled["result"]["workspace"]["workspace_id"])
        # A new server fetches agent manifests online a few seconds after it
        # starts, and fills in the fields about it over a few replies. The
        # capture was made long after. Without a network they stay missing.
        self.cli("agent-manifests", "server", "agent-manifests", "--json", settle=15)
        # Herdr knows `cursor-agent` only from those remote manifests, so this
        # waits for them too.
        detected = self.split(shell)
        self.herdr("pane", "run", detected, os.path.join(SCRATCH, "bin", "cursor-agent"))
        self.cli("pane-get-detected-alias", "pane", "get", detected, settle=15)
        self.cli("pane-get-not-found", "pane", "get", "w99:p1")

        # Two clients, one on the session and one that starts its own server,
        # which is what the process captures were made from.
        attached = self.client(["herdr", "--session", SESSION], {
            "__CFBundleIdentifier": "com.mitchellh.ghostty", "TERM_PROGRAM": "ghostty"})
        plain = self.client(["herdr"], {})
        ours = {str(server.pid), str(attached), str(plain)}
        for _ in range(100):
            spawned = subprocess.run(["pgrep", "-P", str(plain)], capture_output=True, text=True).stdout.split()
            connected = subprocess.run(["lsof", "-b", "-w", "-U", "-a", "-p", str(attached), "-F", "n"],
                                       capture_output=True, text=True).stdout
            if spawned and "n->" in connected:
                break
            time.sleep(0.1)
        else:
            raise SystemExit("scratch clients did not attach")
        ours.update(spawned)

        self.tool("pgrep-herdr-two-sessions", PGREP, keep=ours)
        self.tool("pgrep-herdr-none", ["pgrep", "-a", "-lf", "^([^ ]*/)?herdr-no-such-program( |$)"])
        self.tool("lsof-capture-session-one-client",
                  ["lsof", "-b", "-w", "-U", "-a", "-p", f"{attached},{server.pid}", "-F", "dn"])
        self.tool("ps-env-capture-session-one-client",
                  ["ps", "-Eww", "-o", "pid=,etime=,command=", "-p", str(attached)])
        self.tool("ps-env-server-plain", ["ps", "-Eww", "-o", "command=", "-p", str(server.pid)])

        front = self.tool("lsappinfo-front", ["lsappinfo", "front"]).strip()
        info = self.tool("lsappinfo-bundleid-ghostty", ["lsappinfo", "info", "-only", "bundleid", front])
        self.tool("lsappinfo-bundleid-gone", ["lsappinfo", "info", "-only", "bundleid", "ASN:0x0-0xfffff0:"])
        # One line on macOS 26, an info block with a bundleID= line on 27.
        found = re.search(r'(?:"CFBundleIdentifier"|bundleID)="([^"]+)"', info)
        if not found:
            raise SystemExit(f"no bundle id for the app in front ({front!r}); bring an app to the front and run again")
        bundle = found.group(1)
        # Whatever is in front stands in for Ghostty, and a made-up id for
        # Terminal.app, so neither depends on which apps are open.
        self.tool("lsappinfo-find-ghostty", ["lsappinfo", "find", f"bundleid={bundle}"])
        self.tool("lsappinfo-find-not-running", ["lsappinfo", "find", "bundleid=io.github.dev.no-such-app"])
        self.tool("lsappinfo-visible-process-list", ["lsappinfo", "visibleProcessList"])
        self.tool("open-bundle-unknown", ["open", "-b", "io.github.dev.no-such-app"])
        # The reply is this Mac's appearance, so it's compared with the
        # capture for whichever one is on now.
        appearance = ["defaults", "read", "-g", "AppleInterfaceStyle"]
        dark = subprocess.run(appearance, capture_output=True, text=True).stdout.strip() == "Dark"
        mode, other = ("dark", "light") if dark else ("light", "dark")
        SKIPPED[f"sys/defaults-appearance-{other}"] = f"same command, and macOS is in {mode} mode"
        self.tool(f"defaults-appearance-{mode}", appearance)
        # Its values are this Mac's notification settings and the checkout's
        # path, so only the names are compared. They sit in a 20-character
        # column after a 2-space indent, and "notification centre" fills it.
        self.tool("terminal-notifier-diagnose",
                  [os.path.join(ROOT, "vendor/HerdrNudge.app/Contents/MacOS/terminal-notifier"), "-diagnose"],
                  mask=lambda text: re.sub(r"^(  .{19}) .*$", lambda m: m.group(1).rstrip(), text, flags=re.M))

    def client(self, argv, terminal):
        env = {"HOME": SCRATCH, "PATH": "/usr/bin:/bin:/usr/sbin:/sbin",
               "TERM": "xterm-256color", "LANG": "en_US.UTF-8", **terminal}
        pid, fd = pty.fork()
        if pid == 0:
            # If execve fails, the child must not fall back into this script
            # and run its cleanup.
            try:
                fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
                os.execve(HERDR, argv, env)
            finally:
                os._exit(127)
        self.children.append(pid)
        # Nobody reads the screen, but a full pty would stall the client.
        threading.Thread(target=drain, args=(fd,), daemon=True).start()
        return pid

    def report(self):
        missing = []
        for kind in ("cli", "socket", "sys"):
            bases = {re.sub(r"-\d+\.\d+\.\d+$", "", n[:-5])
                     for n in os.listdir(os.path.join(FIXTURES, kind)) if n.endswith(".json")}
            missing += [f"{kind}/{b}" for b in sorted(bases)
                        if f"{kind}/{b}" not in self.checked and f"{kind}/{b}" not in SKIPPED]
        for label, why in SKIPPED.items():
            print(f"skip  {label}: {why}")
        for label in missing:
            print(f"NEW   {label}: not re-run here yet; add it to verify.py")
        if self.recorded:
            print("\nrecorded:", ", ".join(self.recorded))
        print(f"\n{len(self.checked)} checked, {self.drifted} moved, {len(SKIPPED)} skipped")
        return 1 if self.drifted or missing else 0


def walk(value, path, out):
    if isinstance(value, dict):
        out.add(f"{path or '.'}: object")
        for key, item in value.items():
            child = f"{path}.{key}" if path else key
            walk(item, child, out)
            if key in STABLE and "[]" not in child and not isinstance(item, (dict, list)):
                out.add(f"{child} = {json.dumps(item)}")
    elif isinstance(value, list):
        out.add(f"{path}: array")
        for item in value:
            walk(item, path + "[]", out)
    else:
        kind = {bool: "bool", int: "number", float: "number", str: "string"}.get(type(value), "null")
        out.add(f"{path}: {kind}")


def drain(fd):
    try:
        while os.read(fd, 65536):
            pass
    except OSError:
        pass


def now():
    return datetime.datetime.now().isoformat(timespec="seconds")


SERVER_ENV = {
    "HOME": SCRATCH,
    "USER": os.environ.get("USER", ""),
    "PATH": "/usr/bin:/bin:/usr/sbin:/sbin:" + os.path.dirname(HERDR),
    "HERDR_SESSION": SESSION,
}


def setup():
    if os.path.exists(SCRATCH):
        teardown()
        shutil.rmtree(SCRATCH)
    os.makedirs(os.path.join(SCRATCH, "bin"))
    # Herdr takes a process called claude for the agent (0.9.0 and 0.9.1), so
    # this script passes for one. It sets a title, as the real one does.
    stand_in = os.path.join(SCRATCH, "bin", "claude")
    with open(stand_in, "w") as f:
        f.write("#!/bin/sh\nprintf '\\033]0;verify\\007'\nwhile :; do sleep 1; done\n")
    os.chmod(stand_in, 0o755)
    # A name Herdr's manifests list as an alias of Cursor, not its label.
    alias = os.path.join(SCRATCH, "bin", "cursor-agent")
    with open(alias, "w") as f:
        f.write("#!/bin/sh\nwhile :; do sleep 1; done\n")
    os.chmod(alias, 0o755)


def teardown(children=()):
    # Both servers live under SCRATCH: the session's, and the one the plain
    # client started.
    for env in (SERVER_ENV, {k: v for k, v in SERVER_ENV.items() if k != "HERDR_SESSION"}):
        try:
            subprocess.run([HERDR, "server", "stop"], env=env, capture_output=True, timeout=10)
        except subprocess.TimeoutExpired:
            pass  # the kills below and the rmtree still run
    for pid in children:
        try:
            os.kill(pid, signal.SIGKILL)
            os.waitpid(pid, 0)
        except (ProcessLookupError, ChildProcessError):
            pass


def main():
    if len(sys.argv) > 2 or (len(sys.argv) == 2 and sys.argv[1] != "--record"):
        raise SystemExit(__doc__)
    verify = Verify(record=len(sys.argv) == 2)
    setup()
    try:
        verify.run()
    finally:
        teardown(verify.children)
        shutil.rmtree(SCRATCH, ignore_errors=True)
    print()
    sys.exit(verify.report())


if __name__ == "__main__":
    main()
