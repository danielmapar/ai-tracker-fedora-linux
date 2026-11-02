"""End-to-end tests of the real collector binary (collector/target/release/agent-usage).

Each test runs it in a throwaway HOME with made-up data:
  - Claude Code files: login, account, settings, terminal sessions and today's transcripts;
  - fake `systemctl` / `journalctl` answering from fixture files and recording every call;
  - a fake Codex background service on the real control-socket path (fake_codex.py), and a fake
    `codex` for the private fallback;
  - stand-in processes for the managed Codex service, a stray server and a Claude session.
Nothing touches your real files, services, logins or the network (live Claude limits stay off,
except one test that turns them on together with --offline).

Usage: python3 -I verify/e2e/collector_e2e.py [--bin PATH] [--out DIR]
--out writes the healthy run's stream for the widget's contract test (real-collector.stream).
"""
import json, os, shutil, signal, stat, subprocess, sys, tempfile, time, traceback

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
sys.path.insert(0, HERE)
import fake_codex  # noqa: E402

args = sys.argv[1:]
BIN = args[args.index("--bin") + 1] if "--bin" in args else os.path.join(REPO, "collector/target/release/agent-usage")
OUT = args[args.index("--out") + 1] if "--out" in args else None
TOKEN = "fake-access-token-must-never-leak"
REFRESH = "fake-refresh-token-must-never-leak"
NOW = int(time.time())
MIDNIGHT = int(time.mktime(time.localtime(NOW)[:3] + (0, 0, 0, 0, 0, -1)))


def iso(ts):
    return time.strftime("%Y-%m-%dT%H:%M:%S.000Z", time.gmtime(ts))


class Sandbox:
    def __init__(self):
        # /tmp, not $TMPDIR: unix socket paths must stay under 108 bytes
        self.root = tempfile.mkdtemp(prefix="au-collector-e2e.", dir="/tmp")
        self.home = os.path.join(self.root, "home")
        self.run = os.path.join(self.root, "run")
        self.bin = os.path.join(self.root, "bin")
        self.fx = os.path.join(self.root, "fixtures")
        self.calls = os.path.join(self.root, "calls.log")
        self.procs = []
        for d in (self.home, self.run, self.bin, self.fx):
            os.makedirs(d)
        os.chmod(self.run, 0o700)
        self.codex_cfg = os.path.join(self.root, "codex.json")
        self.codex_log = os.path.join(self.root, "codex-calls.log")
        # The fake background service runs in this process; the private one in the collector's.
        os.environ["FAKE_CODEX_CONFIG"], os.environ["FAKE_CODEX_LOG"] = self.codex_cfg, self.codex_log
        self.set_codex({})
        self._fake_tools()
        self._claude_files()
        self._journals()
        self.daemon = None

    def path(self, *p):
        return os.path.join(self.home, *p)

    def write(self, rel, text, mode=0o644):
        p = self.path(rel)
        os.makedirs(os.path.dirname(p), exist_ok=True)
        with open(p, "w") as f:
            f.write(text if isinstance(text, str) else json.dumps(text))
        os.chmod(p, mode)
        return p

    def set_codex(self, cfg):
        with open(self.codex_cfg, "w") as f:
            json.dump(cfg, f)

    def _script(self, name, body):
        p = os.path.join(self.bin, name)
        with open(p, "w") as f:
            f.write(body)
        os.chmod(p, 0o755)

    def _fake_tools(self):
        calls, fx = self.calls, self.fx
        self._script("systemctl", f"""#!/bin/sh
echo "systemctl $*" >> {calls}
case "$*" in
  "--user show "*) cat {fx}/units.txt ;;
  "is-active sshd") cat {fx}/sshd.txt ;;
  *) exit 1 ;;
esac
""")
        # journalctl: `-o cat` -> the unit's recent lines; `--show-cursor` -> today's lines in
        # short-unix form, with a cursor; after that cursor -> only lines appended since.
        self._script("journalctl", f"""#!/bin/sh
echo "journalctl $*" >> {calls}
unit=$3
case "$*" in
  *" -o cat "*) cat "{fx}/$unit.cat" 2>/dev/null ;;
  *--after-cursor*)
    cur=$(echo "$*" | sed 's/.*--after-cursor \\([^ ]*\\).*/\\1/')
    total=$(wc -l < "{fx}/$unit.unix" 2>/dev/null || echo 0)
    case $cur in c*) ;; *) exit 1 ;; esac
    tail -n +$(( ${{cur#c}} + 1 )) "{fx}/$unit.unix" 2>/dev/null
    echo "-- cursor: c$total" ;;
  *--show-cursor*)
    cat "{fx}/$unit.unix" 2>/dev/null
    echo "-- cursor: c$(wc -l < "{fx}/$unit.unix" 2>/dev/null || echo 0)" ;;
esac
""")
        # Spies: the collector must never run these (it reads files directly).
        self._script("claude", f"""#!/bin/sh
echo "claude $*" >> {calls}
case "$*" in "auth status --json") echo '{{"loggedIn": false}}' ;; *) exit 1 ;; esac
""")
        os.makedirs(self.path(".local/bin"), exist_ok=True)
        with open(self.path(".local/bin/codex"), "w") as f:
            f.write(f"#!/bin/sh\nexec python3 -I {HERE}/fake_codex.py \"$@\"\n")
        os.chmod(self.path(".local/bin/codex"), 0o755)

    def _claude_files(self):
        self.write(".claude/.credentials.json", {"claudeAiOauth": {
            "accessToken": TOKEN, "refreshToken": REFRESH, "expiresAt": (NOW + 3600) * 1000,
            "scopes": ["user:inference", "user:profile", "user:sessions:claude_code"],
            "subscriptionType": "max", "rateLimitTier": "default_claude_max_20x"}}, 0o600)
        self.write(".claude.json", {"oauthAccount": {
            "emailAddress": "user@example.com", "organizationName": "Example Org",
            "organizationRateLimitTier": "default_claude_max_20x"}})
        self.write(".claude/settings.json", {"remoteControlAtStartup": True})
        # today's transcript: two requests (one logged twice: counted once) + one from yesterday
        def entry(ts, mid, rid, i, o, cw, cr):
            return json.dumps({"type": "assistant", "timestamp": iso(ts), "requestId": rid,
                               "message": {"id": mid, "role": "assistant", "usage": {
                                   "input_tokens": i, "output_tokens": o,
                                   "cache_creation_input_tokens": cw, "cache_read_input_tokens": cr}}})
        t = max(MIDNIGHT + 60, NOW - 600)
        lines = [entry(MIDNIGHT - 3600, "m0", "r0", 999, 999, 999, 999),
                 json.dumps({"type": "user", "timestamp": iso(t), "message": {"role": "user", "content": "hi"}}),
                 entry(t, "m1", "r1", 100, 200, 300, 4000),
                 entry(t + 1, "m1", "r1", 100, 200, 300, 4000),
                 entry(t + 2, "m2", "r2", 10, 20, 30, 400)]
        self.write(".claude/projects/-home-user-project/session-1.jsonl", "\n".join(lines) + "\n")

    def _journals(self):
        rc = "claude-remote-control.service"
        wd = "codex-remote-control.service"
        with open(os.path.join(self.fx, f"{rc}.cat"), "w") as f:
            f.write("Remote Control server for my-laptop\n"
                    "\x1b[32m✔ Connected\x1b[0m · my-laptop · Ready\n"
                    "Capacity: 2/32\n"
                    "Refactor auth flow\n"
                    "Fix flaky tests\n"
                    "Continue coding in the Claude app or https://claude.ai/code?environment=env_FAKE123\n")
        drops = [(MIDNIGHT + 3600, 3), (MIDNIGHT + 7200, 14)]
        with open(os.path.join(self.fx, f"{rc}.unix"), "w") as f:
            f.write(f"{MIDNIGHT - 100}.000001 my-laptop claude[11]: [23:58:20] Reconnected after 9s\n")  # yesterday
            for at, secs in drops:
                f.write(f"{at}.000001 my-laptop claude[11]: status line\n")
                f.write(f"{at}.500000 my-laptop claude[11]: [x] Reconnected after {secs}s\n")
        with open(os.path.join(self.fx, f"{wd}.cat"), "w") as f:
            f.write("Starting codex-remote-control.service...\nbackground service is back\n"
                    "Finished codex-remote-control.service.\n")
        with open(os.path.join(self.fx, f"{wd}.unix"), "w") as f:
            for i, msg in enumerate(["relay status=unknown (miss 1 of 3)", "relay status=unknown (miss 2 of 3)",
                                     "relay status=unknown (miss 3 of 3)", "restarting background service",
                                     "connected again"]):
                f.write(f"{MIDNIGHT + 5000 + i}.000001 my-laptop codex-remote-control-watchdog[22]: {msg}\n")
                f.write(f"{MIDNIGHT + 5000 + i}.000002 my-laptop systemd[1]: Finished codex-remote-control.service.\n")
        self.units(claude=True, timer=True)
        with open(os.path.join(self.fx, "sshd.txt"), "w") as f:
            f.write("active\n")

    def units(self, claude=True, timer=True, last_ok=True):
        blocks = [
            f"ActiveState={'active' if claude else 'inactive'}\nActiveEnterTimestamp=@{NOW - 6 * 3600}\nExecMainExitTimestamp=\nResult=success",
            f"ActiveState=inactive\nActiveEnterTimestamp=\nExecMainExitTimestamp=@{NOW - 40}\nResult={'success' if last_ok else 'exit-code'}",
            f"ActiveState={'active' if timer else 'inactive'}\nActiveEnterTimestamp=@{NOW - 9 * 3600}\nExecMainExitTimestamp=\nResult=success",
        ]
        with open(os.path.join(self.fx, "units.txt"), "w") as f:
            f.write("\n\n".join(blocks) + "\n")

    def append_journal(self, unit, lines):
        with open(os.path.join(self.fx, f"{unit}.unix"), "a") as f:
            for at, ident, msg in lines:
                f.write(f"{at}.000001 my-laptop {ident}[33]: {msg}\n")

    def start_daemon(self):
        self.daemon = fake_codex.serve_socket(self.path(".codex/app-server-control/app-server-control.sock"))

    def stop_daemon(self):
        if self.daemon:
            self.daemon.close()
            os.unlink(self.path(".codex/app-server-control/app-server-control.sock"))
            self.daemon = None

    def spawn(self, exe_path, *argv):
        """A stand-in process whose /proc exe is `exe_path` (a copy of bash) and whose arguments
        are `argv`, for the collector's process scan."""
        os.makedirs(os.path.dirname(exe_path), exist_ok=True)
        shutil.copy(shutil.which("bash"), exe_path)
        p = subprocess.Popen([exe_path, "-c", "sleep 600; :", *argv], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                             start_new_session=True)
        self.procs.append(p)
        return p

    def env(self, **extra):
        e = {"HOME": self.home, "PATH": f"{self.bin}:/usr/bin:/bin", "LANG": "C.UTF-8",
             "XDG_RUNTIME_DIR": self.run, "XDG_CONFIG_HOME": self.path(".config"),
             "XDG_CACHE_HOME": self.path(".cache"), "TZ": os.environ.get("TZ", ""),
             "FAKE_CODEX_CONFIG": self.codex_cfg, "FAKE_CODEX_LOG": self.codex_log}
        e.update(extra)
        return {k: v for k, v in e.items() if v != ""}

    def collect(self, *flags, **env):
        """Run the collector; returns (frames, first_frame_seconds, total_seconds)."""
        t0 = time.monotonic()
        p = subprocess.Popen([BIN, *flags], stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=self.env(**env))
        frames, first = [], None
        for line in p.stdout:
            if first is None:
                first = time.monotonic() - t0
            frames.append(json.loads(line))
        p.wait(timeout=30)
        assert p.returncode == 0, f"collector exited {p.returncode}: {p.stderr.read().decode()[:400]}"
        return frames, first, time.monotonic() - t0

    def calls_log(self):
        try:
            with open(self.calls) as f:
                return f.read().splitlines()
        except OSError:
            return []

    def close(self):
        self.stop_daemon()
        for p in self.procs:
            try:
                os.killpg(p.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            p.wait()
        shutil.rmtree(self.root, ignore_errors=True)


# ---------- tiny test runner ----------

RESULTS = []


def test(name):
    def deco(fn):
        RESULTS.append((name, fn))
        return fn
    return deco


def eq(got, want, what):
    assert got == want, f"{what}: got {got!r}, want {want!r}"


def last(frames):
    return frames[-1]


@test("streams: a first frame within 300 ms listing the parts still loading, then a final frame with none")
def _(sb):
    sb.start_daemon()
    # OpenAI-backed calls take a while: they must arrive in later frames, not hold up the first.
    sb.set_codex({"slow": {"account/rateLimits/read": 0.3, "account/usage/read": 0.6}})
    sb.spawn(sb.path(".codex/packages/app-server-daemon/releases/0.0/bin/codex"), "app-server", "--remote-control")
    time.sleep(0.2)
    frames, first, total = sb.collect("--stream")
    assert "codex.tokens" in frames[0]["pending"], frames[0]["pending"]
    assert total > 0.55, f"whole run {total:.2f} s: the slow call was not waited for"
    assert len(frames) >= 2, f"only {len(frames)} frame(s)"
    assert first < 0.3, f"first frame after {first:.3f} s"
    assert frames[0]["pending"], "first frame lists no loading parts"
    eq(frames[-1]["pending"], [], "last frame's pending")
    for a, b in zip(frames, frames[1:]):
        assert set(b["pending"]) <= set(a["pending"]), "a part went back to loading"
    if OUT:
        os.makedirs(OUT, exist_ok=True)
        with open(os.path.join(OUT, "real-collector.stream"), "w") as f:
            for i, fr in enumerate(frames):
                f.write(f"{0.05 if i == 0 else 0.3} {json.dumps(fr)}\n")


@test("Claude account: email, organisation and the Ultra plan (Max 20x login), read from files")
def _(sb):
    a = last(sb.collect()[0])["claude"]["account"]
    eq((a["logged_in"], a["email"], a["org"], a["plan"], a["auth_method"]),
       (True, "user@example.com", "Example Org", "ultra", "claude.ai"), "account")
    assert not any(c.startswith("claude ") for c in sb.calls_log()), "ran the claude CLI"


@test("Claude plan: a Max 5x login stays Max")
def _(sb):
    p = sb.path(".claude/.credentials.json")
    d = json.load(open(p))
    d["claudeAiOauth"]["rateLimitTier"] = "default_claude_max_5x"
    sb.write(".claude/.credentials.json", d, 0o600)
    sb.write(".claude.json", {"oauthAccount": {"emailAddress": "user@example.com"}})
    eq(last(sb.collect()[0])["claude"]["account"]["plan"], "max", "plan")


@test("Claude live limits are off by default: no network call, a calm 'off' marker")
def _(sb):
    eq(last(sb.collect()[0])["claude"]["usage"], {"off": True}, "usage")


@test("Claude live limits on but unreachable: the last good numbers come back marked stale")
def _(sb):
    sb.write(".config/ai-tracker/config.json", {"claude_live_limits": True})
    cache = sb.path(".cache/agent-usage")
    os.makedirs(cache, mode=0o700, exist_ok=True)
    saved = {"windows": [{"label": "Weekly", "pct": 32.0, "resets_at": NOW + 86400}], "extra_usage": None, "fetched_at": NOW - 300}
    sb.write(".cache/agent-usage/claude-usage-last.json", saved, 0o600)
    u = last(sb.collect("--offline")[0])["claude"]["usage"]
    assert "offline" in u.get("error", ""), u
    eq(u["stale"]["windows"][0]["pct"], 32.0, "saved Weekly %")


@test("Claude tokens today: counted from today's transcripts, duplicates once, yesterday excluded")
def _(sb):
    t = last(sb.collect()[0])["claude"]["tokens"]
    eq((t["today_uncached_input"], t["today_output"], t["today_total"]), (110, 220, 110 + 220 + 330 + 4400), "tokens")


@test("Claude remote server: state, capacity, session titles and the claude.ai link from its journal")
def _(sb):
    s = last(sb.collect()[0])["claude"]["workers"]["server"]
    eq((s["service_active"], s["state"], s["capacity"], s["sessions"], s["link"]),
       (True, "Connected", "2/32", ["Refactor auth flow", "Fix flaky tests"], "https://claude.ai/code?environment=env_FAKE123"),
       "server")


@test("Claude remote server: today's drops from 'Reconnected after' lines; yesterday's ignored")
def _(sb):
    drops = last(sb.collect()[0])["claude"]["workers"]["server"]["drops_today"]
    eq([d["secs"] for d in drops], [3, 14], "drops")


@test("journals are read incrementally: a second run adds only new lines, never counts twice")
def _(sb):
    sb.collect()
    sb.append_journal("claude-remote-control.service", [(NOW - 5, "claude", "[x] Reconnected after 6s")])
    sb.append_journal("codex-remote-control.service", [(NOW - 4, "codex-remote-control-watchdog", "restarting background service")])
    w = last(sb.collect()[0])
    eq([d["secs"] for d in w["claude"]["workers"]["server"]["drops_today"]], [3, 14, 6], "drops after append")
    eq(w["codex"]["workers"]["watchdog"]["events_today"]["restarts"], 2, "restarts after append")
    assert any("--after-cursor" in c for c in sb.calls_log()), "second run did not use the saved cursor"


@test("Claude terminal sessions: live ones counted, the one not on Remote Control named")
def _(sb):
    a = sb.spawn(os.path.join(sb.root, "procs/claude"), "claude")
    b = sb.spawn(os.path.join(sb.root, "procs2/claude"), "claude")
    dead = 4_000_000  # no such pid: ignored
    sb.write(".claude/sessions/1.json", {"kind": "interactive", "entrypoint": "cli", "pid": a.pid, "bridgeSessionId": "b1", "name": "one"})
    sb.write(".claude/sessions/2.json", {"kind": "interactive", "entrypoint": "cli", "pid": b.pid, "name": "api-refactor"})
    sb.write(".claude/sessions/3.json", {"kind": "interactive", "entrypoint": "cli", "pid": dead, "name": "gone"})
    sb.write(".claude/sessions/4.json", {"kind": "background", "entrypoint": "cli", "pid": a.pid, "name": "bg"})
    w = last(sb.collect()[0])["claude"]["workers"]
    eq(w["sessions"], {"total": 2, "connected": 1, "not_connected": ["api-refactor"]}, "sessions")
    eq(w["auto_connect"], True, "auto-connect")


@test("Codex via its background service: account, limits, credits, token totals and phone link")
def _(sb):
    sb.start_daemon()
    c = last(sb.collect()[0])["codex"]
    eq((c["account"]["email"], c["account"]["plan"]), ("codex-user@example.com", "plus"), "account")
    eq([(w["label"], w["pct"]) for w in c["usage"]["windows"]], [("5-hour", 7), ("Weekly", 11)], "windows")
    eq(c["usage"]["credits"], "500", "credits")
    eq((c["tokens"]["today"], c["tokens"]["last_7_days"], c["tokens"]["lifetime"]),
       (1_000_000, sum(range(1, 8)) * 1_000_000, 1_840_000_000), "tokens")
    eq(c["workers"]["service"]["relay"], {"status": "connected", "name": "my-laptop"}, "relay")


@test("Codex token totals fail: the last good totals come back marked stale")
def _(sb):
    sb.start_daemon()
    sb.collect()
    sb.set_codex({"fail": {"account/usage/read": "simulated outage"}})
    t = last(sb.collect()[0])["codex"]["tokens"]
    assert "simulated outage" in t.get("error", ""), t
    eq(t["stale"]["lifetime"], 1_840_000_000, "saved lifetime")


@test("Codex limits fail: the last good limits come back marked stale")
def _(sb):
    sb.start_daemon()
    sb.collect()
    sb.set_codex({"fail": {"account/rateLimits/read": "simulated outage"}})
    u = last(sb.collect()[0])["codex"]["usage"]
    assert "simulated outage" in u.get("error", ""), u
    eq(len(u["stale"]["windows"]), 2, "saved windows")


@test("Codex service not running: a private, short-lived app-server answers instead")
def _(sb):
    c = last(sb.collect()[0])["codex"]
    eq(c["account"]["email"], "codex-user@example.com", "account via private app-server")
    eq(c["workers"]["service"]["relay"], {"status": "service not running"}, "relay")
    eq(c["workers"]["service"]["running"], False, "running")


@test("Codex processes: the managed service (with its start time) and a stray server are found")
def _(sb):
    # The scan sees every process of this user, so real Codex servers count too: compare to a baseline.
    base = last(sb.collect()[0])["codex"]["workers"]
    eq(base["service"]["running"], False, "managed service before the stand-in starts")
    sb.spawn(sb.path(".codex/packages/app-server-daemon/releases/0.0/bin/codex"), "app-server", "--remote-control", "--listen", "unix://")
    sb.spawn(os.path.join(sb.root, "elsewhere/codex"), "app-server", "--listen", "unix:///tmp/x.sock")
    sb.spawn(os.path.join(sb.root, "proxy/codex"), "app-server", "proxy")  # SSH proxies are not servers
    time.sleep(0.2)
    w = last(sb.collect()[0])["codex"]["workers"]
    eq((w["service"]["running"], w["stray_servers"] - base["stray_servers"]), (True, 1), "running, new strays")
    assert abs(w["service"]["since"] - NOW) < 120, w["service"]["since"]


@test("Codex watchdog: timer, last run, result, last message and today's restarts and misses")
def _(sb):
    sb.units(timer=True, last_ok=False)
    wd = last(sb.collect()[0])["codex"]["workers"]["watchdog"]
    eq((wd["timer_active"], wd["last_ok"], wd["last_message"]), (True, False, "background service is back"), "watchdog")
    eq(wd["last_run"], NOW - 40, "last run")
    eq((wd["events_today"]["restarts"], wd["events_today"]["relay_misses"]), (1, 3), "today's counts")
    eq(wd["relay_held_by"], None, "held by")


@test("Codex phone link held by Codex Desktop: read from the watchdog's note, ignored once stale")
def _(sb):
    held = os.path.join(sb.run, "codex-remote-control-watchdog.held")
    with open(held, "w") as f:
        f.write("Codex Desktop\n")
    eq(last(sb.collect()[0])["codex"]["workers"]["watchdog"]["relay_held_by"], "Codex Desktop", "fresh note")
    os.utime(held, (NOW - 600, NOW - 600))
    eq(last(sb.collect()[0])["codex"]["workers"]["watchdog"]["relay_held_by"], None, "stale note")


@test("Claude server down and watchdog timer off are reported as such")
def _(sb):
    sb.units(claude=False, timer=False)
    w = last(sb.collect()[0])
    eq(w["claude"]["workers"]["server"]["service_active"], False, "server active")
    eq(w["codex"]["workers"]["watchdog"]["timer_active"], False, "timer active")


@test("signed out of Claude Code: reported as signed out")
def _(sb):
    os.unlink(sb.path(".claude/.credentials.json"))
    eq(last(sb.collect()[0])["claude"]["account"]["logged_in"], False, "logged in")


@test("read-only: only `systemctl show/is-active` and `journalctl` reads; no login, logout or service change")
def _(sb):
    sb.start_daemon()
    sb.collect("--stream")
    calls = sb.calls_log()
    bad = [c for c in calls if not (c.startswith("systemctl --user show ") or c == "systemctl is-active sshd" or c.startswith("journalctl --user -u "))]
    eq(bad, [], "unexpected commands")
    codex = open(sb.codex_log).read()
    assert "account/usage/read" in codex, "the fake Codex service saw no calls"
    assert "UNEXPECTED" not in codex, codex
    methods = set(codex.split())
    assert methods <= {"initialize", "account/read", "account/rateLimits/read", "account/usage/read", "remoteControl/status/read"}, methods


@test("secrets: the login token never reaches the output or the cache; cache is private (0700/0600)")
def _(sb):
    sb.start_daemon()
    frames, _, _ = sb.collect("--stream")
    blob = json.dumps(frames)
    assert TOKEN not in blob and REFRESH not in blob, "token in output"
    cache = sb.path(".cache/agent-usage")
    eq(stat.S_IMODE(os.stat(cache).st_mode), 0o700, "cache dir mode")
    for name in os.listdir(cache):
        p = os.path.join(cache, name)
        eq(stat.S_IMODE(os.stat(p).st_mode), 0o600, f"{name} mode")
        with open(p, "rb") as f:
            data = f.read()
        assert TOKEN.encode() not in data and REFRESH.encode() not in data, f"token in {name}"


@test("saved snapshot: the last complete run is saved for the widget's instant first open")
def _(sb):
    frames, _, _ = sb.collect("--stream")
    snap = json.load(open(sb.path(".cache/agent-usage/last-snapshot.json")))
    eq(snap["claude"]["account"]["email"], "user@example.com", "saved account")
    sb2 = frames[-1]
    eq(snap["codex"]["workers"]["stray_servers"], sb2["codex"]["workers"]["stray_servers"], "saved workers")


@test("the next run's first frame starts from the saved values of the slow parts")
def _(sb):
    sb.start_daemon()
    sb.collect("--stream")
    sb.set_codex({"slow": {"account/usage/read": 1.5}})
    frames, _, _ = sb.collect("--stream")
    assert "codex.tokens" in frames[0]["pending"], frames[0]["pending"]
    eq(frames[0]["codex"]["tokens"]["lifetime"], 1_840_000_000, "placeholder from the saved snapshot")


@test("plain mode (no --stream): one complete JSON object, same data")
def _(sb):
    sb.start_daemon()
    frames, _, _ = sb.collect()
    eq(len(frames), 1, "objects printed")
    assert "pending" not in frames[0] or frames[0]["pending"] == [], frames[0].get("pending")
    eq(frames[0]["codex"]["account"]["email"], "codex-user@example.com", "codex account")


def main():
    assert os.access(BIN, os.X_OK), f"no collector binary at {BIN} (cargo build --release)"
    passed = failed = 0
    for name, fn in RESULTS:
        sb = Sandbox()
        t0 = time.monotonic()
        try:
            fn(sb)
            passed += 1
            print(f"PASS {name}  ({(time.monotonic() - t0) * 1000:.0f} ms)")
        except Exception as e:  # noqa: BLE001
            failed += 1
            print(f"FAIL {name}: {e}")
            if not isinstance(e, AssertionError):
                traceback.print_exc()
        finally:
            sb.close()
    print(f"collector e2e: {passed} passed, {failed} failed")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
