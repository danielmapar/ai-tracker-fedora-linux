import copy, json, sys, time, pathlib
sample = json.load(open(sys.argv[1]))
out = pathlib.Path(sys.argv[2])
now = int(time.time())
H, D = 3600, 86400

def base():
    s = copy.deepcopy(sample)
    s["generated_at"] = now
    s["claude"]["usage"]["windows"] = [{"label": "Weekly", "pct": 24.0, "resets_at": now + 3*D + 22*H}]
    s["codex"]["usage"]["windows"] = [{"label": "Weekly", "pct": 11.0, "resets_at": now + 3*D + 5*H}]
    s["codex"]["workers"]["watchdog"]["last_run"] = now - 17*60
    return s

sc = {}
sc["healthy"] = base()

s = base()  # two windows each, still healthy
s["claude"]["usage"]["windows"] = [
    {"label": "5-hour", "pct": 38.0, "resets_at": now + 2*H + 10*60},
    {"label": "Weekly", "pct": 24.0, "resets_at": now + 3*D + 22*H}]
s["codex"]["usage"]["windows"] = [
    {"label": "5-hour", "pct": 7.0, "resets_at": now + 4*H + 2*60},
    {"label": "Weekly", "pct": 11.0, "resets_at": now + 3*D + 5*H}]
sc["two"] = s

s = base()  # heavy: near limits and remote-access trouble
s["claude"]["usage"]["windows"] = [
    {"label": "5-hour", "pct": 92.0, "resets_at": now + 1*H + 12*60},
    {"label": "Weekly", "pct": 74.0, "resets_at": now + 2*D + 4*H},
    {"label": "Weekly · Opus", "pct": 31.0, "resets_at": now + 2*D + 4*H}]
s["claude"]["usage"]["extra_usage"] = {"pct": 45.0, "used": 2250, "limit": 5000, "currency": "USD"}
s["claude"]["workers"]["server"]["state"] = "Reconnecting"
s["claude"]["workers"]["sessions"] = {"connected": 2, "not_connected": ["api-refactor"], "total": 3}
s["claude"]["workers"]["auto_connect"] = False
s["codex"]["usage"]["windows"] = [
    {"label": "5-hour", "pct": 100.0, "resets_at": now + 38*60},
    {"label": "Weekly", "pct": 81.0, "resets_at": now + 1*D + 3*H}]
s["codex"]["usage"]["limit_reached"] = "primary"
s["codex"]["workers"]["stray_servers"] = 1
s["codex"]["workers"]["watchdog"]["last_ok"] = False
s["codex"]["workers"]["watchdog"]["last_message"] = "stray app-server found; restarting service"
s["codex"]["workers"]["service"]["relay"]["status"] = "disconnected"
s["codex"]["workers"]["ssh"]["active"] = False
sc["heavy"] = s

s = base()  # logged out everywhere
s["claude"]["account"] = {"logged_in": False}
s["claude"]["usage"] = {"error": "Not signed in to Claude Code"}
s["claude"]["tokens"]["today_total"] = 0; s["claude"]["tokens"]["today_output"] = 0; s["claude"]["tokens"]["today_uncached_input"] = 0
s["claude"]["workers"]["server"] = {"service_active": False, "state": None, "capacity": None, "link": None, "sessions": []}
s["claude"]["workers"]["sessions"] = {"connected": 0, "not_connected": [], "total": 0}
s["codex"]["account"] = {"logged_in": False}
s["codex"]["usage"] = {"error": "Codex is not reachable"}
s["codex"]["tokens"] = None
s["codex"]["workers"]["service"] = {"running": False, "relay": {"status": "service not running"}}
sc["loggedout"] = s

s = base()  # API error + empty windows + service down
s["claude"]["usage"] = {"error": "claude.ai is rate-limiting usage checks. Try Refresh again in a minute.", "stale": {"windows": [{"label": "Weekly", "pct": 25.0, "resets_at": now + 3*D + 21*H}], "extra_usage": None, "fetched_at": now - 7*60}}
s["claude"]["workers"]["server"]["service_active"] = False
s["codex"]["usage"]["windows"] = []
s["codex"]["usage"]["credits"] = "unlimited"
sc["apierror"] = s

s = base()  # many windows (layout stress)
s["claude"]["usage"]["windows"] = [
    {"label": "5-hour", "pct": 12.0, "resets_at": now + 4*H},
    {"label": "Weekly", "pct": 55.0, "resets_at": now + 1*D + 1*H},
    {"label": "Weekly · Opus", "pct": 71.0, "resets_at": now + 1*D + 1*H},
    {"label": "Weekly · Sonnet", "pct": 18.0, "resets_at": now + 1*D + 1*H},
    {"label": "Weekly · apps", "pct": 0.0, "resets_at": None},
    {"label": "Weekly · Cowork", "pct": 3.0, "resets_at": now + 6*D}]
s["codex"]["usage"]["windows"] = [
    {"label": "5-hour", "pct": 20.0, "resets_at": now + 3*H},
    {"label": "Weekly", "pct": 40.0, "resets_at": now + 4*D},
    {"label": "5-hour · gpt-5-codex-mini", "pct": 2.0, "resets_at": now + 3*H},
    {"label": "Weekly · gpt-5-codex-mini", "pct": 9.0, "resets_at": now + 4*D}]
sc["many"] = s

s = base()  # codex section missing, claude workers missing
s["codex"] = None
s["claude"]["workers"] = None
sc["partial"] = s

for k, v in sc.items():
    (out / f"{k}.json").write_text(json.dumps(v))
(out / "garbage.json").write_text("this is not json {")
print("wrote", len(sc) + 1, "scenarios")

# ---------- v3 ----------
def line(delay, obj):
    return f"{delay} {json.dumps(obj)}"

H6 = 6*H + 40*60
def with_history(s):
    srv = s["claude"]["workers"]["server"]
    srv["since"] = now - H6
    srv["drops_today"] = [{"at": now - 3*H, "secs": 3}, {"at": now - 2*H - 30*60, "secs": 14}, {"at": now - 40*60, "secs": 6}]
    svc = s["codex"]["workers"]["service"]
    svc["since"] = now - 7*H
    s["codex"]["workers"]["watchdog"]["events_today"] = {"restarts": 2, "relay_misses": 3, "recent": [
        {"at": now - 5*H, "msg": "relay status=unknown (miss 2 of 3)"},
        {"at": now - 5*H + 60, "msg": "relay status=unknown (miss 3 of 3)"},
        {"at": now - 5*H + 61, "msg": "restarting background service"},
        {"at": now - 4*H - 20*60, "msg": "background service not running; starting it"},
        {"at": now - 4*H - 20*60 + 3, "msg": "background service is back"}]}
    return s

hist = with_history(copy.deepcopy(sc["two"]))
(out / "history.json").write_text(json.dumps(hist))

# streamed: saved numbers first (dimmed, "updating…"), then Claude limits, then Codex
old = copy.deepcopy(hist)
old["claude"]["usage"]["windows"] = [
    {"label": "5-hour", "pct": 21.0, "resets_at": now + 2*H + 10*60},
    {"label": "Weekly", "pct": 22.0, "resets_at": now + 3*D + 22*H}]
old["codex"]["usage"]["windows"] = [{"label": "5-hour", "pct": 3.0, "resets_at": now + 4*H}, {"label": "Weekly", "pct": 9.0, "resets_at": now + 3*D + 5*H}]
old["codex"]["tokens"]["today"] = 1000000
f1 = copy.deepcopy(old); f1["pending"] = ["claude.usage", "codex.usage", "codex.tokens"]
f2 = copy.deepcopy(old); f2["claude"]["usage"] = hist["claude"]["usage"]; f2["pending"] = ["codex.usage", "codex.tokens"]
f3 = copy.deepcopy(hist); f3["pending"] = []
(out / "stream.stream").write_text("\n".join([line(0.05, f1), line(1.2, f2), line(1.2, f3)]) + "\n")

# first ever run: nothing saved yet, so loading parts show skeletons
g1 = copy.deepcopy(hist); g1["claude"]["usage"] = None; g1["codex"]["usage"] = None; g1["codex"]["tokens"] = None
g1["claude"]["workers"]["server"]["drops_today"] = None
g1["pending"] = ["claude.usage", "codex.usage", "codex.tokens", "claude.drops"]
(out / "firstever.stream").write_text("\n".join([line(0.05, g1), line(1.6, f3)]) + "\n")

# collector dies after the first frame
(out / "dies.stream").write_text(line(0.05, f1) + "\n")

# heavy + history: every fix button
hv = with_history(copy.deepcopy(sc["heavy"]))
hv["codex"]["workers"]["service"]["since"] = now - 25*60
(out / "fixes.json").write_text(json.dumps(hv))
(out / "snapshot-heavy.json").write_text(json.dumps(hv))

# live Claude limits turned off (the default for new installs)
off = copy.deepcopy(sc["two"]); off["claude"]["usage"] = {"off": True}
(out / "claudeoff.json").write_text(json.dumps(off))

# ---------- demo.gif (verify/demo.sh) ----------
before = copy.deepcopy(hist)
before["claude"]["usage"]["windows"] = [
    {"label": "5-hour", "pct": 31.0, "resets_at": now + 2*H + 10*60},
    {"label": "Weekly", "pct": 22.0, "resets_at": now + 3*D + 22*H}]
before["codex"]["usage"]["windows"] = [
    {"label": "5-hour", "pct": 4.0, "resets_at": now + 4*H + 2*60},
    {"label": "Weekly", "pct": 10.0, "resets_at": now + 3*D + 5*H}]
before["codex"]["tokens"]["today"] = 21800000
(out / "demo-before.json").write_text(json.dumps(before))
d1 = copy.deepcopy(before); d1["pending"] = ["claude.usage", "codex.usage", "codex.tokens"]
d2 = copy.deepcopy(before); d2["claude"]["usage"] = hist["claude"]["usage"]; d2["pending"] = ["codex.usage", "codex.tokens"]
d3 = copy.deepcopy(hist); d3["pending"] = []
(out / "demo-stream.stream").write_text("\n".join([line(0.05, d1), line(1.1, d2), line(0.6, d3)]) + "\n")
problem = copy.deepcopy(hist)
problem["codex"]["workers"]["service"]["relay"]["status"] = "disconnected"
problem["codex"]["workers"]["stray_servers"] = 1
(out / "demo-problem.json").write_text(json.dumps(problem))
fixed = copy.deepcopy(hist)
fixed["codex"]["workers"]["service"]["since"] = now + 8
ev = fixed["codex"]["workers"]["watchdog"]["events_today"]
ev["restarts"] += 1
ev["recent"] += [{"at": now + 6, "msg": "unmanaged app-server (4242) is blocking the background service; replacing it"},
                 {"at": now + 9, "msg": "background service is back"}]
fixed["codex"]["workers"]["watchdog"]["last_message"] = "background service is back"
(out / "demo-healthy.json").write_text(json.dumps(fixed))

# Codex slow or unreachable: limits and token totals fall back to the last good values
cs = copy.deepcopy(sc["two"])
cs["codex"]["usage"] = {"error": "Could not read limits: timeout",
                        "stale": {**sc["two"]["codex"]["usage"], "fetched_at": now - 25*60}}
cs["codex"]["tokens"] = {"error": "Codex token totals did not load: timeout",
                         "stale": {**sc["two"]["codex"]["tokens"], "fetched_at": now - 25*60}}
(out / "codexstale.json").write_text(json.dumps(cs))

# Ultra (the renamed Max 20x): the login says "max" and the collector reports "ultra"; no 5-hour
# window, and a separate weekly Fable limit
ul = copy.deepcopy(sc["healthy"])
ul["claude"]["account"]["plan"] = "ultra"
ul["claude"]["usage"]["windows"] = [
    {"label": "Weekly", "pct": 32.0, "resets_at": now + 3*D + 4*H},
    {"label": "Weekly · Fable", "pct": 0.0, "resets_at": now + 3*D + 4*H}]
(out / "ultra.json").write_text(json.dumps(ul))

# Codex Desktop holds the relay under the background service's identity: the service is refused
# (relay "errored") but the phone reaches this computer through the desktop app
cd = copy.deepcopy(sc["two"])
cd["codex"]["workers"]["service"]["relay"] = {"name": "my-laptop", "status": "errored"}
cd["codex"]["workers"]["watchdog"]["relay_held_by"] = "Codex Desktop"
cd["codex"]["workers"]["watchdog"]["last_message"] = "phone link held by Codex Desktop on this computer; not restarting"
(out / "codexdesktop.json").write_text(json.dumps(cd))
