#!/bin/sh
# Visual + behavioural check of extension/ai-tracker@danielmapar/ui.js in a REAL GNOME Shell 50, sandboxed:
#   headless compositor (no window on your desktop), private D-Bus session, throwaway HOME/XDG
#   dirs, keyfile GSettings, a fake collector serving scenarios from verify/sample.json and a fake
#   systemctl. It runs as your user and can still see the system bus (logind).
# It never touches ~/.local/share/gnome-shell, ~/.local/bin, dconf or systemd.
# Output: verify/out/*.png and verify/out/shell.log.   Usage: sh verify/run.sh [font-size]
# verify/demo.sh reuses this with AU_HARNESS (driver script), AU_OUT_DIR, AU_SNAPSHOT (scenario saved
# as the "last snapshot") and AU_BACKGROUND (solid desktop colour) to record docs/demo.gif.
# verify/e2e.sh adds AU_TIMEOUT (seconds, default 260) and AU_EXTRA_SCENARIOS (a folder of extra
# scenario files, e.g. the real collector's output from verify/e2e/collector_e2e.py).
set -eu
V=$(cd "$(dirname "$0")" && pwd)
UI=$(dirname "$V")/extension/ai-tracker@danielmapar   # the extension source
FONT=${1:-14}
OUT=${AU_OUT_DIR:-$V/out}
BOX=$(mktemp -d "${TMPDIR:-/tmp}/au-ui-verify.XXXXXX")
RUN=/run/user/$(id -u)/au-$$          # short: Wayland socket paths must be < 108 bytes
EXT=$BOX/home/.local/share/gnome-shell/extensions/agent-usage-test@local
# Session helpers that D-Bus started (some in their own process group) can still write into the
# sandbox while they exit, so stop anything using its runtime folder and remove it until it stays gone.
cleanup() {
    set +e # the shell is usually gone already: a failing kill must not end the cleanup (set -e)
    [ -n "${PID:-}" ] && kill -TERM -"$PID" 2>/dev/null
    for _ in 1 2 3 4 5 6 7 8; do
        for p in /proc/[0-9]*; do
            grep -qz "^XDG_RUNTIME_DIR=$RUN\$" "$p/environ" 2>/dev/null && kill -TERM "${p#/proc/}" 2>/dev/null
        done
        fusermount3 -u -z "$RUN/doc" 2>/dev/null || fusermount -u -z "$RUN/doc" 2>/dev/null
        rm -rf "$BOX" "$RUN" 2>/dev/null
        sleep 1
        [ -e "$BOX" ] || [ -e "$RUN" ] || break
    done
}
trap cleanup EXIT
mkdir -p "$OUT" "$EXT" "$BOX/home/.local/bin" "$BOX/home/scenarios" "$BOX/home/.config/glib-2.0/settings" "$RUN"
chmod 700 "$RUN"; rm -f "$OUT"/*.png "$OUT/DONE"
cp "$UI/ui.js" "$UI/stylesheet.css" "$EXT/"
cp "${AU_HARNESS:-$V/harness-extension.js}" "$EXT/extension.js"
printf '{"uuid":"agent-usage-test@local","name":"AU harness","description":"test","shell-version":["50"]}' > "$EXT/metadata.json"
install -m 755 "$V/fake-collector.sh" "$BOX/home/.local/bin/agent-usage"
install -m 755 "$(dirname "$V")/app/ai-tracker" "$BOX/home/.local/bin/ai-tracker"   # the real app script
mkdir -p "$BOX/home/fakebin" && install -m 755 "$V"/fakebin/* "$BOX/home/fakebin/"
python3 -I "$V/scenarios.py" "$V/sample.json" "$BOX/home/scenarios" >/dev/null
[ -n "${AU_EXTRA_SCENARIOS:-}" ] && [ -d "$AU_EXTRA_SCENARIOS" ] && { cp "$AU_EXTRA_SCENARIOS"/* "$BOX/home/scenarios/" 2>/dev/null || true; }
# a saved snapshot from an earlier login (heavy: the badge must show before any fetch)
mkdir -p "$BOX/home/.cache/agent-usage" && cp "$BOX/home/scenarios/${AU_SNAPSHOT:-snapshot-heavy}.json" "$BOX/home/.cache/agent-usage/last-snapshot.json"
ICONS=${AU_ICONS:-$UI/icons}   # real icons for the second half
mkdir -p "$BOX/home/icons-src"; cp "$ICONS"/*.svg "$BOX/home/icons-src/" 2>/dev/null || true
cat > "$BOX/home/.config/glib-2.0/settings/keyfile" <<KEY
[org/gnome/shell]
enabled-extensions=['agent-usage-test@local']
disable-user-extensions=false
welcome-dialog-last-shown-version='999'
[org/gnome/desktop/interface]
color-scheme='prefer-dark'
font-name='Adwaita Sans $FONT'
enable-hot-corners=false
${AU_BACKGROUND:+[org/gnome/desktop/background]
picture-options='none'
color-shading-type='solid'
primary-color='$AU_BACKGROUND'}
KEY
B=$BOX/home
setsid env -i PATH="$BOX/home/fakebin:/usr/bin:/bin" HOME="$B" USER="$(id -un)" LOGNAME="$(id -un)" LANG=C.UTF-8 \
  XDG_RUNTIME_DIR="$RUN" XDG_CONFIG_HOME="$B/.config" XDG_DATA_HOME="$B/.local/share" \
  XDG_CACHE_HOME="$B/.cache" XDG_STATE_HOME="$B/.local/state" GSETTINGS_BACKEND=keyfile AU_OUT="$OUT" AU_E2E_ONLY="${AU_E2E_ONLY:-}" \
  timeout "${AU_TIMEOUT:-260}" dbus-run-session -- gnome-shell --headless --wayland --no-x11 --virtual-monitor 1600x1200 \
  > "$OUT/shell.log" 2>&1 &
PID=$!
i=0; while [ ! -f "$OUT/DONE" ] && [ $i -lt "${AU_TIMEOUT:-260}" ] && kill -0 $PID 2>/dev/null; do i=$((i+1)); sleep 1; done
kill -TERM -$PID 2>/dev/null || true; sleep 2; kill -KILL -$PID 2>/dev/null || true
echo "result: $(cat "$OUT/DONE" 2>/dev/null || echo 'no DONE marker')  ($i s)"
echo "collector runs: $(grep -c start "$B/scenarios/runs.log" 2>/dev/null || echo 0)  refresh triggers: $(grep -c 'AUHARNESS refresh' "$OUT/shell.log")"
echo "fix commands run (fake):"; sed "s/^/  /" "$B/scenarios/actions.log" 2>/dev/null || echo "  none"
echo "JS errors/criticals:"; grep -E 'JS ERROR|JS WARNING|CRITICAL' "$OUT/shell.log" | grep -v 'NM.Object' || echo "  none"
ls "$OUT"/*.png | wc -l | xargs echo "screenshots:"
