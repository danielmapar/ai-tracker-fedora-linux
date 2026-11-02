#!/usr/bin/env bash
# Tests app/ai-tracker (the "AI Tracker" app: open / quit the widget) against a fake GNOME Shell:
# a fake `gdbus` that answers GNOME's extensions interface and the widget's Open from files, and a
# fake `notify-send`. It never touches your real GNOME session or settings.
# (verify/e2e/widget-e2e.js also runs the real script against a real headless GNOME Shell.)
set -u
REPO=$(cd "$(dirname "$0")/.." && pwd)
APP="$REPO/app/ai-tracker"
T=$(mktemp -d); trap 'rm -rf "$T"' EXIT
export FAKE="$T/state" PATH="$T/bin:$PATH"
mkdir -p "$FAKE" "$T/bin"

cat > "$T/bin/gdbus" <<'EOF'
#!/bin/sh
# Fake gdbus: GNOME Shell's extensions interface and the widget's Open, from files in $FAKE.
[ -e "$FAKE/down" ] && { echo "Error: GDBus.Error:org.freedesktop.DBus.Error.ServiceUnknown" >&2; exit 1; }
method='' args=''
while [ $# -gt 0 ]; do
    case "$1" in
        --method) method=$2; shift 2 ;;
        --object-path | --dest | --timeout) shift 2 ;;
        call | --session) shift ;;
        *) args="$args $1"; shift ;;
    esac
done
echo "${method##*.}$args" >> "$FAKE/calls"
state=$(cat "$FAKE/ext")
case "$method" in
    org.gnome.Shell.Extensions.GetExtensionInfo)
        if [ "$state" = unknown ]; then echo '(@a{sv} {},)'
        else echo "({'uuid': <'ai-tracker@danielmapar'>, 'name': <'AI Tracker'>, 'state': <$state.0>},)"; fi ;;
    org.freedesktop.DBus.Properties.Get) echo "(<$(cat "$FAKE/user-extensions")>,)" ;;
    org.gnome.Shell.Extensions.EnableExtension)
        [ "$state" = unknown ] && { echo '(false,)'; exit 0; }
        echo 1 > "$FAKE/ext"; echo '(true,)' ;;
    org.gnome.Shell.Extensions.DisableExtension)
        [ "$state" = unknown ] && { echo '(false,)'; exit 0; }
        echo 2 > "$FAKE/ext"; echo '(true,)' ;;
    io.github.danielmapar.AiTracker.Open)
        # The widget's object exists only while it runs, and appears after `menu-delay` tries.
        n=$(( $(cat "$FAKE/opens" 2>/dev/null || echo 0) + 1 )); echo "$n" > "$FAKE/opens"
        if [ "$state" = 1 ] && [ "$n" -gt "$(cat "$FAKE/menu-delay")" ]; then echo '()'; exit 0; fi
        echo "Error: GDBus.Error:org.freedesktop.DBus.Error.UnknownObject" >&2; exit 1 ;;
    *) echo "unexpected method $method" >&2; exit 1 ;;
esac
EOF
printf '#!/bin/sh\nprintf "%%s\\n" "$*" >> "$FAKE/notes"\n' > "$T/bin/notify-send"
chmod +x "$T/bin/gdbus" "$T/bin/notify-send"

pass=0 fail=0
check() { if [ "$1" = "$2" ]; then pass=$((pass + 1)); echo "PASS $3"; else fail=$((fail + 1)); echo "FAIL $3: got [$1], want [$2]"; fi; }
has() { case "$1" in *"$2"*) pass=$((pass + 1)); echo "PASS $3" ;; *) fail=$((fail + 1)); echo "FAIL $3: [$2] not in [$1]" ;; esac; }
setup() { # extension state, user extensions on?, tries before the menu answers
    rm -f "$FAKE"/*
    echo "$1" > "$FAKE/ext"; echo "${2:-true}" > "$FAKE/user-extensions"; echo "${3:-0}" > "$FAKE/menu-delay"
}
run() { out=$("$APP" "$@" 2>&1); code=$?; }
calls() { tr '\n' ' ' < "$FAKE/calls" 2>/dev/null | sed 's/ $//'; }
notes() { cat "$FAKE/notes" 2>/dev/null; }
count() { grep -c "^$1" "$FAKE/calls" 2>/dev/null || true; }

# 1. Running: just open its menu. Nothing is turned on twice, nothing else is called.
setup 1; run
check "$code" 0 "running: exits 0"
check "$(calls)" "GetExtensionInfo ai-tracker@danielmapar Open" "running: only opens the menu"
check "$(notes)" "" "running: no notification"
# 2. Quit earlier: turn it back on, then wait for the widget's menu to appear and open it.
setup 2 true 3; run
check "$code" 0 "quit: exits 0"
check "$(count EnableExtension)" 1 "quit: turned on once"
check "$(cat "$FAKE/ext")" 1 "quit: running afterwards"
check "$(cat "$FAKE/opens")" 4 "quit: keeps asking until the menu answers"
# 3. Loaded but never turned on this session (INITIALIZED): same as quit.
setup 6; run
check "$code/$(count EnableExtension)" "0/1" "initialized: turned on and opened"
# 4. --quit: turns it off, does not open anything.
setup 1; run --quit
check "$code" 0 "--quit: exits 0"
check "$(calls)" "DisableExtension ai-tracker@danielmapar" "--quit: only turns it off"
check "$(cat "$FAKE/ext")" 2 "--quit: off afterwards"
# 5. Opening twice in a row: still one widget (the second open only opens the menu).
setup 2; run; run
check "$(count EnableExtension)/$(count Open)" "1/2" "twice: turned on once, menu opened twice"
# 6. The session started before AI Tracker was installed: say how to fix it, change nothing.
setup unknown; run
check "$code" 1 "unknown: exits 1"
has "$(notes)" "Log out and back in once" "unknown: notification says to log in again"
check "$(count EnableExtension)" 0 "unknown: nothing turned on"
setup unknown; run --quit
check "$code" 1 "unknown --quit: exits 1"
has "$out" "nothing to quit" "unknown --quit: says so"
# 7. All extensions switched off in the Extensions app: say so instead of failing silently.
setup 2 false; run
check "$code/$(count EnableExtension)" "1/0" "extensions off: exits 1, nothing turned on"
has "$(notes)" "Extensions are turned off" "extensions off: notification"
# 8. Broken or unsupported: explain, do not retry.
setup 3; run
check "$code/$(count EnableExtension)/$(count Open)" "1/0/0" "error state: no enable, no open"
has "$(notes)" "hit an error" "error state: notification"
setup 4; run
has "$(notes)" "does not support this GNOME Shell version" "out of date: notification"
# 9. GNOME Shell not answering (not a GNOME session).
setup 1; touch "$FAKE/down"; run
check "$code" 1 "no shell: exits 1"
has "$out" "GNOME Shell is not answering" "no shell: says so"
# 10. Turned on but the menu never answers: give up after about 5 s with a hint.
setup 2 true 999; start=$(date +%s); run; took=$(( $(date +%s) - start ))
check "$code" 1 "no menu: exits 1"
has "$(notes)" "Click its icon in the top bar" "no menu: points to the icon"
check "$([ "$took" -ge 4 ] && [ "$took" -le 12 ] && echo ok || echo "$took s")" ok "no menu: gives up after about 5 s"
# 11. Run from a terminal: the message goes to the terminal, no notification needed.
setup unknown; python3 -I -c 'import pty, sys; pty.spawn(sys.argv[1:])' "$APP" > "$T/tty.txt" 2>&1 < /dev/null || true
has "$(cat "$T/tty.txt")" "Log out and back in once" "terminal: message printed"
check "$(notes)" "" "terminal: no notification"
# 12. Options.
run --bogus
check "$code" 2 "unknown option: exits 2"
run --help
has "$out" "ai-tracker --quit" "--help: shows usage"
# 13. The desktop entry: valid, starts the script, and has a Quit action.
sed "s|@BIN@|$T/bin|g" "$REPO/app/io.github.danielmapar.AiTracker.desktop.in" > "$T/io.github.danielmapar.AiTracker.desktop"
check "$(desktop-file-validate "$T/io.github.danielmapar.AiTracker.desktop" 2>&1)" "" "desktop entry: valid"
check "$(grep -c "^Exec=\"$T/bin/ai-tracker\"\$" "$T/io.github.danielmapar.AiTracker.desktop")" 1 "desktop entry: opens the app"
check "$(grep -c "^Exec=\"$T/bin/ai-tracker\" --quit\$" "$T/io.github.danielmapar.AiTracker.desktop")" 1 "desktop entry: Quit action"
check "$(python3 -I -c 'import sys, xml.dom.minidom as m; m.parse(sys.argv[1]); print("ok")' "$REPO/app/io.github.danielmapar.AiTracker.svg")" ok "icon: well-formed SVG"

echo "app: $pass passed, $fail failed"
[ "$fail" = 0 ]
