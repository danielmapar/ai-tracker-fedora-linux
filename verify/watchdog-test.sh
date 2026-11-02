#!/usr/bin/env bash
# Tests remote-workers/codex-remote-control-watchdog in a sandbox: a fake HOME with a fake Codex
# (status from a file, restarts recorded), fake Codex log/state databases, a fake `systemd-run`,
# and a stand-in "managed service" process. It never touches your real Codex, services or files.
set -u
REPO=$(cd "$(dirname "$0")/.." && pwd)
T=$(mktemp -d); trap 'pkill -P "$SVC" 2>/dev/null; kill "$SVC" 2>/dev/null; rm -rf "$T"' EXIT
export HOME="$T/home" XDG_RUNTIME_DIR="$T/run"
mkdir -p "$HOME/.local/bin" "$HOME/.codex/packages/app-server-daemon" "$XDG_RUNTIME_DIR" "$T/bin"
chmod 700 "$XDG_RUNTIME_DIR"
cat > "$HOME/.local/bin/codex" <<'EOF'
#!/bin/sh
case "$*" in
  "remote-control start --json") printf '{"status":"%s"}\n' "$(cat "$HOME/status")" ;;
  *) echo "$*" >> "$HOME/codex-calls" ;;
esac
EOF
printf '#!/bin/sh\nwhile [ "$1" != "--" ]; do shift; done; shift; exec "$@"\n' > "$T/bin/systemd-run"
chmod +x "$HOME/.local/bin/codex" "$T/bin/systemd-run"
export PATH="$T/bin:$PATH"
# The stand-in service: a copy of bash under the managed path, with the service's arguments.
cp "$(command -v bash)" "$HOME/.codex/packages/app-server-daemon/codex"
"$HOME/.codex/packages/app-server-daemon/codex" -c 'sleep 600; :' app-server --remote-control 2>/dev/null &
SVC=$!
sleep 0.3
pass=0 fail=0
check() { if [ "$1" = "$2" ]; then pass=$((pass + 1)); echo "PASS $3"; else fail=$((fail + 1)); echo "FAIL $3: got [$1], want [$2]"; fi; }
db() { python3 -I - "$@" <<'EOF'
import sqlite3, sys, time
home, what = sys.argv[1], sys.argv[2]
if what == "logs":
    c = sqlite3.connect(f"{home}/.codex/logs_2.sqlite")
    c.execute("create table if not exists logs (id integer primary key, ts integer, process_uuid text, feedback_log_body text)")
    c.execute("delete from logs")
    if sys.argv[3] != "none":
        body = 'failed to connect: HTTP error: 409 Conflict, body: {"detail":"Remote app server already online"}'
        c.execute("insert into logs (ts, process_uuid, feedback_log_body) values (?, ?, ?)", (int(time.time()) - int(sys.argv[4]), f"pid:{sys.argv[3]}:ab", body))
else:
    c = sqlite3.connect(f"{home}/.codex/state_5.sqlite")
    c.execute("create table if not exists remote_control_enrollments (app_server_client_name text, server_id text, remote_control_enabled integer)")
    c.execute("delete from remote_control_enrollments")
    c.executemany("insert into remote_control_enrollments values (?, ?, 1)", [("", "srv"), ("Codex Desktop", "srv"), ("Other", "other-srv")])
c.commit()
EOF
}
run() { bash "$REPO/remote-workers/codex-remote-control-watchdog" 2>&1; }
held() { cat "$XDG_RUNTIME_DIR/codex-remote-control-watchdog.held" 2>/dev/null; }
restarts() { grep -c 'daemon restart' "$HOME/codex-calls" 2>/dev/null || echo 0; }

pgrep -u "$(id -u)" -f 'app-server --remote-control' | grep -qx "$SVC" || { echo "stand-in service did not start"; exit 1; }
db "$HOME" state

# 1. Refused with 409 while Codex Desktop holds the link: note it once, never restart.
echo errored > "$HOME/status"; db "$HOME" logs "$SVC" 20
check "$(run)" "phone link held by Codex Desktop on this computer; not restarting" "409: names the holder"
check "$(held)" "Codex Desktop" "409: writes the holder note"
check "$(run)" "" "409 again: stays quiet"
check "$(restarts)" "0" "409: no restart"
# 2. The link comes back: say so, drop the note.
echo connected > "$HOME/status"
check "$(run)" "connected again" "connected: reports recovery"
check "$(held)" "" "connected: note removed"
check "$(run)" "" "connected again: quiet"
# 3. A 409 older than 3 minutes is not evidence: back to normal misses, restart on the third.
echo errored > "$HOME/status"; db "$HOME" logs "$SVC" 400
check "$(run)" "relay status=errored (miss 1 of 3)" "old 409: counts a miss"
db "$HOME" logs none
check "$(run)" "relay status=errored (miss 2 of 3)" "no 409: second miss"
check "$(run | tail -n 1)" "restarting background service" "no 409: restarts on the third miss"
check "$(restarts)" "1" "no 409: exactly one restart"
# 4. Misses, then a 409: the note replaces the miss count; a later plain miss starts from 1.
db "$HOME" logs none; run >/dev/null
db "$HOME" logs "$SVC" 5
check "$(run)" "phone link held by Codex Desktop on this computer; not restarting" "miss then 409: holder"
db "$HOME" logs none
check "$(run)" "relay status=errored (miss 1 of 3)" "409 gone, still errored: counting restarts from 1"
check "$(held)" "" "409 gone: note removed"
# 5. A 409 from another process (an old service pid) does not count.
db "$HOME" logs 999999 5
check "$(run)" "relay status=errored (miss 2 of 3)" "409 from another pid: ignored"
# 6. No state database: a generic holder name.
rm -f "$HOME/.codex/state_5.sqlite"; rm -f "$XDG_RUNTIME_DIR/codex-remote-control-watchdog.misses"
db "$HOME" logs "$SVC" 5
check "$(run)" "phone link held by another Codex app on this computer; not restarting" "no state db: generic name"
check "$(restarts)" "1" "no extra restarts"

echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ]
