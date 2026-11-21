"""End-to-end tests of the real collector binary (collector/target/release/agent-usage).

Each test runs it in a throwaway HOME with made-up data:
  - Claude Code files: login, account, settings, terminal sessions and today's transcripts;
  - Gemini (Antigravity) files: settings, today's brain transcripts, fake `secret-tool` keyring and
    a local loopback HTTP server mocking Cloud Code `loadCodeAssist` + `retrieveUserQuota`;
  - fake `systemctl` / `journalctl` answering from fixture files and recording every call;
  - a fake Codex background service on the real control-socket path (fake_codex.py), and a fake
    `codex` for the private fallback;
  - stand-in processes for the managed Codex service, a stray server, Claude sessions and `agy` sessions.
Nothing touches your real files, services, logins or the network.

Usage: python3 -I verify/e2e/collector_e2e.py [--bin PATH] [--out DIR]
--out writes the healthy run's stream for the widget's contract test (real-collector.stream).
"""
import http.server, json, os, shutil, signal, stat, subprocess, sys, tempfile, threading, time, traceback

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
sys.path.insert(0, HERE)
import fake_codex  # noqa: E402

args = sys.argv[1:]
BIN = args[args.index("--bin") + 1] if "--bin" in args else os.path.join(REPO, "collector/target/release/agent-usage")
OUT = args[args.index("--out") + 1] if "--out" in args else None
TOKEN = "fake-access-token-must-never-leak"
REFRESH = "fake-refresh-token-must-never-leak"
GEMINI_TOKEN = "fake-gemini-access-token-must-never-leak"
GEMINI_REFRESH = "fake-gemini-refresh-token-must-never-leak"
NOW = int(time.time())
MIDNIGHT = int(time.mktime(time.localtime(NOW)[:3] + (0, 0, 0, 0, 0, -1)))
SLEEPER = None


def sleeper_bin():
    global SLEEPER
    if SLEEPER is None:
        fd, path = tempfile.mkstemp(prefix="au-e2e-sleeper.", dir="/tmp")
        os.close(fd)
        rustc = shutil.which("rustc") or os.path.expanduser("~/.cargo/bin/rustc")
        subprocess.run([rustc, "-O", "-", "-o", path],
                       input=b"fn main() { loop { std::thread::park(); } }\n", check=True)
        SLEEPER = path
    return SLEEPER


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
        self.gemini_reqs = []
        self.gemini_cfg = {}
        self._start_gemini_http()
        self._fake_tools()
        self._claude_files()
        self._gemini_files()
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

    def set_gemini_secret(self, obj):
        p = os.path.join(self.fx, "gemini-secret.json")
        if obj is None:
            if os.path.exists(p):
                os.unlink(p)
        else:
            with open(p, "w") as f:
                json.dump(obj, f)

    def _start_gemini_http(self):
        sb = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_args):
                pass

            def do_POST(self):
                length = int(self.headers.get("Content-Length", "0"))
                body = self.rfile.read(length).decode("utf-8", errors="replace")
                ua = self.headers.get("User-Agent", "")
                auth = self.headers.get("Authorization", "")
                sb.gemini_reqs.append({"path": self.path, "ua": ua, "auth": auth, "body": body})
                if ua != "antigravity/1.5":
                    self.send_response(403)
                    self.end_headers()
                    return
                if auth != f"Bearer {GEMINI_TOKEN}":
                    self.send_response(401)
                    self.end_headers()
                    return
                delay = sb.gemini_cfg.get("delay", 0)
                if delay:
                    time.sleep(delay)
                status = sb.gemini_cfg.get("status", 200)
                if status != 200:
                    self.send_response(status)
                    self.end_headers()
                    return
                if self.path == "/v1internal:loadCodeAssist":
                    resp = sb.gemini_cfg.get("loadCodeAssist") or {
                        "currentTier": {"id": "standard-tier", "name": "Gemini Code Assist"},
                        "paidTier": {
                            "id": "g1-ultra-tier",
                            "name": "Google AI Ultra",
                            "availableCredits": [{"creditType": "GOOGLE_ONE_AI", "creditAmount": "500"}],
                        },
                    }
                elif self.path == "/v1internal:retrieveUserQuota":
                    resp = sb.gemini_cfg.get("retrieveUserQuota") or {
                        "buckets": [
                            {"modelId": "gemini-4-carbon-high", "remainingFraction": 0.78, "resetTime": iso(NOW + 7200)},
                            {"modelId": "gemini-3-flash", "remainingFraction": 0.78, "resetTime": iso(NOW + 7200)},
                            {"modelId": "claude-sonnet-4-5", "remainingFraction": 0.65, "resetTime": iso(NOW + 5400)},
                            {"modelId": "gpt-oss-120b", "remainingFraction": 0.80, "resetTime": iso(NOW + 7200)},
                            {"modelId": "tab_flash_lite_preview", "remainingFraction": 0.10, "resetTime": iso(NOW + 3600)},
                        ]
                    }
                else:
                    self.send_response(404)
                    self.end_headers()
                    return
                payload = json.dumps(resp).encode("utf-8")
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

        self.gemini_srv = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.gemini_port = self.gemini_srv.server_address[1]
        self.gemini_thr = threading.Thread(target=self.gemini_srv.serve_forever, daemon=True)
        self.gemini_thr.start()

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
        self._script("secret-tool", f"""#!/bin/sh
echo "secret-tool $*" >> {calls}
case "$*" in
  "lookup service gemini username antigravity")
    if [ -f "{fx}/gemini-secret.json" ]; then
      cat "{fx}/gemini-secret.json"
    else
      exit 1
    fi ;;
  *) exit 1 ;;
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

    def _gemini_files(self):
        self.write(".gemini/antigravity-cli/settings.json", {"modelProvider": "antigravity"})
        self.set_gemini_secret({
            "token": {
                "access_token": GEMINI_TOKEN,
                "refresh_token": GEMINI_REFRESH,
                "expiry": time.strftime("%Y-%m-%dT%H:%M:%S.615636742Z", time.gmtime(NOW + 3600)),
            },
            "auth_method": "consumer",
            "id_token_email": "gemini-user@example.com",
        })
        t = max(MIDNIGHT + 60, NOW - 600)
        conv1 = [
            json.dumps({"step_index": 0, "type": "PLANNER_RESPONSE", "created_at": iso(MIDNIGHT - 3600),
                        "input_tokens": 999, "output_tokens": 999, "cache_read_tokens": 999}),
            json.dumps({"step_index": 1, "type": "USER_INPUT", "created_at": iso(t), "content": "hello"}),
            json.dumps({"step_index": 2, "type": "PLANNER_RESPONSE", "created_at": iso(t + 1),
                        "input_tokens": 200, "output_tokens": 50, "cache_creation_tokens": 100, "cache_read_tokens": 1000}),
            json.dumps({"step_index": 2, "type": "PLANNER_RESPONSE", "created_at": iso(t + 2),
                        "input_tokens": 200, "output_tokens": 50, "cache_creation_tokens": 100, "cache_read_tokens": 1000}),
            json.dumps({"step_index": 3, "type": "PLANNER_RESPONSE", "created_at": iso(t + 3),
                        "input_tokens": 30, "output_tokens": 10, "cache_read_tokens": 500}),
        ]
        self.write(".gemini/antigravity-cli/brain/conv-1/.system_generated/logs/transcript.jsonl",
                   "\n".join(conv1) + "\n")
        # Must NEVER be scanned (only exact `transcript.jsonl`):
        self.write(".gemini/antigravity-cli/brain/conv-1/.system_generated/logs/transcript_full.jsonl",
                   json.dumps({"step_index": 99, "type": "PLANNER_RESPONSE", "created_at": iso(t + 4),
                               "input_tokens": 500_000, "output_tokens": 500_000}) + "\n")
        # Same step_index=2 in another conversation must NOT collide:
        self.write(".gemini/antigravity-cli/brain/conv-2/.system_generated/logs/transcript.jsonl",
                   json.dumps({"step_index": 2, "type": "PLANNER_RESPONSE", "created_at": iso(t + 5),
                               "input_tokens": 70, "output_tokens": 40, "cache_read_tokens": 200}) + "\n")

    def _journals(self):
        rc = "claude-remote-control.service"
        wd = "codex-remote-control.service"
        gd = "antigravity-cli-daemon.service"
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
        with open(os.path.join(self.fx, f"{gd}.cat"), "w") as f:
            f.write("[RemoteControl] Starting Antigravity daemon\n"
                    "[RemoteControl] Instance name: my-laptop\n"
                    "\x1b[32m✔ Connected\x1b[0m · https://antigravity.google.com/remote/env_GEMINI1\n")
        with open(os.path.join(self.fx, f"{gd}.unix"), "w") as f:
            f.write(f"{MIDNIGHT - 200}.000001 my-laptop agy[44]: [RemoteControl] Reconnected after 8s\n")  # yesterday
            f.write(f"{MIDNIGHT + 1800}.000001 my-laptop agy[44]: [RemoteControl] Reconnected after 5s\n")
            f.write(f"{MIDNIGHT + 4000}.000001 my-laptop agy[44]: [RemoteControl] stream disconnected, Reconnecting\n")
            f.write(f"{MIDNIGHT + 4011}.000001 my-laptop agy[44]: [RemoteControl] Connected\n")
        self.units(claude=True, timer=True, gemini=True)
        with open(os.path.join(self.fx, "sshd.txt"), "w") as f:
            f.write("active\n")

    def units(self, claude=True, timer=True, last_ok=True, gemini=True):
        blocks = [
            f"ActiveState={'active' if claude else 'inactive'}\nActiveEnterTimestamp=@{NOW - 6 * 3600}\nExecMainExitTimestamp=\nResult=success",
            f"ActiveState=inactive\nActiveEnterTimestamp=\nExecMainExitTimestamp=@{NOW - 40}\nResult={'success' if last_ok else 'exit-code'}",
            f"ActiveState={'active' if timer else 'inactive'}\nActiveEnterTimestamp=@{NOW - 9 * 3600}\nExecMainExitTimestamp=\nResult=success",
            f"ActiveState={'active' if gemini else 'inactive'}\nActiveEnterTimestamp={'@' + str(NOW - 4 * 3600) if gemini else ''}\nExecMainExitTimestamp=\nResult=success",
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
        """A stand-in process whose /proc exe is `exe_path` and whose exact arguments are `argv`,
        for the collector's process scan."""
        os.makedirs(os.path.dirname(exe_path), exist_ok=True)
        shutil.copy(sleeper_bin(), exe_path)
        p = subprocess.Popen([exe_path, *argv], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                             start_new_session=True)
        self.procs.append(p)
        return p

    def env(self, **extra):
        e = {"HOME": self.home, "PATH": f"{self.bin}:/usr/bin:/bin", "LANG": "C.UTF-8",
             "XDG_RUNTIME_DIR": self.run, "XDG_CONFIG_HOME": self.path(".config"),
             "XDG_CACHE_HOME": self.path(".cache"), "TZ": os.environ.get("TZ", ""),
             "FAKE_CODEX_CONFIG": self.codex_cfg, "FAKE_CODEX_LOG": self.codex_log,
             "AGENT_USAGE_GEMINI_URL": f"http://127.0.0.1:{self.gemini_port}"}
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
        self.gemini_srv.shutdown()
        self.gemini_srv.server_close()
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
    sb.spawn(os.path.join(sb.root, "agy-stream/agy"), "--remote-control")
    time.sleep(0.2)
    frames, first, total = sb.collect("--stream")
    assert "codex.tokens" in frames[0]["pending"], frames[0]["pending"]
    assert total > 0.55, f"whole run {total:.2f} s: the slow call was not waited for"
    assert len(frames) >= 2, f"only {len(frames)} frame(s)"
    assert first < 0.3, f"first frame after {first:.3f} s"
    assert frames[0]["pending"], "first frame lists no loading parts"
    eq(frames[-1]["pending"], [], "last frame's pending")
    eq(frames[-1]["gemini"]["account"]["plan"], "ultra", "healthy gemini plan in stream")
    eq(len(frames[-1]["gemini"]["usage"]["windows"]), 2, "healthy gemini windows in stream")
    assert frames[-1]["gemini"]["workers"]["sessions"]["connected"] >= 1, frames[-1]["gemini"]["workers"]["sessions"]
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
    sb.append_journal("antigravity-cli-daemon.service", [(NOW - 3, "agy", "[RemoteControl] Reconnected after 4s")])
    w = last(sb.collect()[0])
    eq([d["secs"] for d in w["claude"]["workers"]["server"]["drops_today"]], [3, 14, 6], "claude drops after append")
    eq(w["codex"]["workers"]["watchdog"]["events_today"]["restarts"], 2, "codex restarts after append")
    eq([d["secs"] for d in w["gemini"]["workers"]["daemon"]["drops_today"]], [5, 11, 4], "gemini drops after append")
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


@test("Gemini account, quota windows and G1 credits via keyring + Cloud Code API")
def _(sb):
    g = last(sb.collect()[0])["gemini"]
    eq((g["account"]["logged_in"], g["account"]["email"], g["account"]["plan"], g["account"]["auth_method"]),
       (True, "gemini-user@example.com", "ultra", "google"), "gemini account")
    eq([(w["label"], w["pct"], w["resets_at"]) for w in g["usage"]["windows"]],
       [("5-hour · Gemini", 22.0, NOW + 7200), ("5-hour · Claude & GPT", 35.0, NOW + 5400)],
       "gemini windows")
    eq(g["usage"]["credits"], 500, "gemini credits")
    paths = {r["path"] for r in sb.gemini_reqs}
    eq(paths, {"/v1internal:loadCodeAssist", "/v1internal:retrieveUserQuota"}, "Cloud Code endpoints called")
    assert all(r["ua"] == "antigravity/1.5" for r in sb.gemini_reqs), sb.gemini_reqs


@test("Gemini API key mode: logged in as apikey, empty windows, no keyring or Cloud Code calls")
def _(sb):
    sb.write(".gemini/antigravity-cli/settings.json", {"modelProvider": "gemini"})
    g = last(sb.collect(GEMINI_API_KEY="AIzaSyFakeKey")[0])["gemini"]
    eq(g["account"], {"logged_in": True, "email": None, "plan": None, "auth_method": "apikey"}, "apikey account")
    eq(g["usage"], {"windows": [], "credits": None}, "apikey usage")
    eq(sb.gemini_reqs, [], "no HTTP requests in apikey mode")
    assert not any(c.startswith("secret-tool ") for c in sb.calls_log()), "ran secret-tool in apikey mode"


@test("Gemini stale fallback when quota endpoint rate-limits or --offline, keeping plan")
def _(sb):
    sb.collect()  # populate gemini-usage-last.json and last-snapshot.json
    sb.gemini_cfg["status"] = 429
    g429 = last(sb.collect()[0])["gemini"]
    assert "rate-limiting" in g429["usage"].get("error", ""), g429["usage"]
    eq(g429["usage"]["stale"]["credits"], 500, "stale credits on 429")
    eq(len(g429["usage"]["stale"]["windows"]), 2, "stale windows on 429")
    eq(g429["account"]["plan"], "ultra", "account plan preserved on 429")

    os.unlink(sb.path(".cache/agent-usage/last-snapshot.json"))
    goff = last(sb.collect("--offline")[0])["gemini"]
    assert "offline" in goff["usage"].get("error", ""), goff["usage"]
    eq(goff["account"]["plan"], "ultra", "account plan restored from stale usage when offline")


@test("Gemini live limits opt-out: calm 'off' marker and zero Cloud Code requests")
def _(sb):
    sb.write(".config/ai-tracker/config.json", {"gemini_live_limits": False})
    g = last(sb.collect()[0])["gemini"]
    eq(g["usage"], {"off": True}, "gemini usage off")
    eq(sb.gemini_reqs, [], "no HTTP requests when gemini_live_limits is false")


@test("Gemini tokens today: transcript.jsonl counted, transcript_full.jsonl ignored, step_index deduped per file")
def _(sb):
    t = last(sb.collect()[0])["gemini"]["tokens"]
    eq((t["today_uncached_input"], t["today_output"], t["today_total"]),
       (300, 100, 300 + 100 + 100 + 1700), "gemini tokens")


@test("Gemini remote daemon & agy terminal sessions: journal state, drops and /proc session classification")
def _(sb):
    base = last(sb.collect()[0])["gemini"]["workers"]["sessions"]
    sb.spawn(os.path.join(sb.root, "agy1/agy"), "--remote-control")
    sb.spawn(os.path.join(sb.root, "agy2/agy"), "--model", "models")  # --model value must NOT count as subcommand
    sb.spawn(os.path.join(sb.root, "agy3/agy"), "models")            # subcommand: ignored
    sb.spawn(os.path.join(sb.root, "agy4/agy"), "remote-control")    # daemon subcommand: ignored
    sb.spawn(os.path.join(sb.root, "agy5/agy"), "-p", "summarize")   # headless prompt: ignored
    time.sleep(0.2)
    w = last(sb.collect()[0])["gemini"]["workers"]
    d = w["daemon"]
    eq((d["service_active"], d["since"], d["state"], d["instance_name"], d["link"]),
       (True, NOW - 4 * 3600, "Connected", "my-laptop", "https://antigravity.google.com/remote/env_GEMINI1"),
       "gemini daemon")
    eq([x["secs"] for x in d["drops_today"]], [5, 11], "gemini drops")
    s = w["sessions"]
    eq((s["total"] - base["total"], s["connected"] - base["connected"]), (2, 1), "agy sessions delta")


@test("Claude server down, Gemini daemon down and watchdog timer off are reported as such")
def _(sb):
    sb.units(claude=False, timer=False, gemini=False)
    w = last(sb.collect()[0])
    eq(w["claude"]["workers"]["server"]["service_active"], False, "claude server active")
    eq(w["codex"]["workers"]["watchdog"]["timer_active"], False, "codex timer active")
    eq((w["gemini"]["workers"]["daemon"]["service_active"], w["gemini"]["workers"]["daemon"]["since"]),
       (False, None), "gemini daemon inactive")


@test("signed out of Claude Code and Gemini: reported as signed out")
def _(sb):
    os.unlink(sb.path(".claude/.credentials.json"))
    sb.set_gemini_secret(None)
    w = last(sb.collect()[0])
    eq(w["claude"]["account"]["logged_in"], False, "claude logged in")
    eq(w["gemini"]["account"]["logged_in"], False, "gemini logged in")
    assert "Not signed in" in w["gemini"]["usage"].get("error", ""), w["gemini"]["usage"]


@test("read-only: only `systemctl show/is-active`, `journalctl` and `secret-tool lookup` reads")
def _(sb):
    sb.start_daemon()
    sb.collect("--stream")
    calls = sb.calls_log()
    bad = [
        c for c in calls
        if not (
            c.startswith("systemctl --user show ")
            or c == "systemctl is-active sshd"
            or c.startswith("journalctl --user -u ")
            or c == "secret-tool lookup service gemini username antigravity"
        )
    ]
    eq(bad, [], "unexpected commands")
    codex = open(sb.codex_log).read()
    assert "account/usage/read" in codex, "the fake Codex service saw no calls"
    assert "UNEXPECTED" not in codex, codex
    methods = set(codex.split())
    assert methods <= {"initialize", "account/read", "account/rateLimits/read", "account/usage/read", "remoteControl/status/read"}, methods


@test("secrets: login tokens never reach the output or the cache; cache is private (0700/0600)")
def _(sb):
    sb.start_daemon()
    frames, _, _ = sb.collect("--stream")
    blob = json.dumps(frames)
    for secret in (TOKEN, REFRESH, GEMINI_TOKEN, GEMINI_REFRESH):
        assert secret not in blob, f"secret leaked in output: {secret}"
    cache = sb.path(".cache/agent-usage")
    eq(stat.S_IMODE(os.stat(cache).st_mode), 0o700, "cache dir mode")
    for name in os.listdir(cache):
        p = os.path.join(cache, name)
        eq(stat.S_IMODE(os.stat(p).st_mode), 0o600, f"{name} mode")
        with open(p, "rb") as f:
            data = f.read()
        for secret in (TOKEN, REFRESH, GEMINI_TOKEN, GEMINI_REFRESH):
            assert secret.encode() not in data, f"secret in {name}: {secret}"


@test("saved snapshot: the last complete run is saved for the widget's instant first open")
def _(sb):
    frames, _, _ = sb.collect("--stream")
    snap = json.load(open(sb.path(".cache/agent-usage/last-snapshot.json")))
    eq(snap["claude"]["account"]["email"], "user@example.com", "saved claude account")
    eq((snap["gemini"]["account"]["email"], snap["gemini"]["account"]["plan"]),
       ("gemini-user@example.com", "ultra"), "saved gemini account")
    sb2 = frames[-1]
    eq(snap["codex"]["workers"]["stray_servers"], sb2["codex"]["workers"]["stray_servers"], "saved workers")


@test("the next run's first frame starts from the saved values of the slow parts (including Gemini plan)")
def _(sb):
    sb.start_daemon()
    sb.collect("--stream")
    sb.set_codex({"slow": {"account/usage/read": 1.5}})
    sb.gemini_cfg["delay"] = 0.4
    frames, _, _ = sb.collect("--stream")
    assert "codex.tokens" in frames[0]["pending"], frames[0]["pending"]
    assert "gemini.usage" in frames[0]["pending"], frames[0]["pending"]
    eq(frames[0]["codex"]["tokens"]["lifetime"], 1_840_000_000, "placeholder from the saved snapshot")
    eq(frames[0]["gemini"]["account"]["plan"], "ultra", "Frame 1 gemini plan reused from saved snapshot")
    eq(frames[0]["gemini"]["usage"]["credits"], 500, "Frame 1 gemini usage placeholder from saved snapshot")


@test("plain mode (no --stream): one complete JSON object, same data")
def _(sb):
    sb.start_daemon()
    frames, _, _ = sb.collect()
    eq(len(frames), 1, "objects printed")
    assert "pending" not in frames[0] or frames[0]["pending"] == [], frames[0].get("pending")
    eq(frames[0]["codex"]["account"]["email"], "codex-user@example.com", "codex account")
    eq(frames[0]["gemini"]["account"]["email"], "gemini-user@example.com", "gemini account")


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
    if SLEEPER and os.path.exists(SLEEPER):
        os.unlink(SLEEPER)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
