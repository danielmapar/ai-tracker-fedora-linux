#!/usr/bin/env bash
# Build and install AI Tracker for the current user:
#   collector  -> ~/.local/bin/agent-usage                                   (Rust, via cargo)
#   extension  -> ~/.local/share/gnome-shell/extensions/ai-tracker@danielmapar
# Usage: ./install.sh [--workers]
#   --workers  also install the remote-access workers the widget monitors (systemd user units, see
#              remote-workers/). They are installed but not switched on: turning them on lets your
#              Claude / ChatGPT account start work on this computer, so that is your explicit step.
# The UI is installed under a new build name each time, so once the loader is live (after the first
# login with this extension.js) a running shell picks up a new build with a disable/enable.
set -euo pipefail

REPO=$(cd "$(dirname "$0")" && pwd)
UUID=ai-tracker@danielmapar
SRC="$REPO/extension/$UUID"
DEST="${XDG_DATA_HOME:-$HOME/.local/share}/gnome-shell/extensions/$UUID"
BIN="$HOME/.local/bin"
CARGO=${CARGO:-$(command -v cargo || echo "$HOME/.cargo/bin/cargo")}
WORKERS=0
for a in "$@"; do
    case "$a" in
        --workers) WORKERS=1 ;;
        -h | --help) sed -n '2,9p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown option: $a (try --help)" >&2; exit 2 ;;
    esac
done

echo "==> collector"
[ -x "$CARGO" ] || { echo "cargo not found: install Rust from https://rustup.rs" >&2; exit 1; }
"$CARGO" build --release --quiet --manifest-path "$REPO/collector/Cargo.toml"
mkdir -p "$BIN"
install -m 755 "$REPO/collector/target/release/agent-usage" "$BIN/agent-usage"
echo "    $BIN/agent-usage"

echo "==> extension"
mkdir -p "$DEST/icons"
# extension.js is the loader. Replace it only when it changed: a new loader needs a re-login.
loader_changed=0
if ! cmp -s "$SRC/extension.js" "$DEST/extension.js"; then
    install -m 644 "$SRC/extension.js" "$DEST/extension.js"
    loader_changed=1
fi
install -m 644 "$SRC/metadata.json" "$SRC/stylesheet.css" "$SRC/ui.js" "$DEST/"
install -m 644 "$SRC"/icons/*.svg "$DEST/icons/"
build="ui-$(date +%Y%m%d%H%M%S).js"
install -m 644 "$SRC/ui.js" "$DEST/$build"
printf '%s\n' "$build" > "$DEST/ui-current.tmp"
mv "$DEST/ui-current.tmp" "$DEST/ui-current"
# Older builds are no longer referenced (ui.js stays as the loader's fallback).
find "$DEST" -maxdepth 1 -name 'ui-*.js' ! -name "$build" -delete
echo "    $DEST ($build)"

# Enabled at the next login even if this shell has never seen the extension.
python3 -I - "$UUID" <<'EOF' || echo "    could not update GNOME settings here; enable it later with: gnome-extensions enable $UUID"
import ast, subprocess, sys
uuid = sys.argv[1]
raw = subprocess.check_output(["gsettings", "get", "org.gnome.shell", "enabled-extensions"], text=True)
cur = ast.literal_eval(raw.replace("@as ", "").strip())
if uuid not in cur:
    subprocess.check_call(["gsettings", "set", "org.gnome.shell", "enabled-extensions", str(cur + [uuid])])
    print(f"    enabled {uuid}")
EOF

if [ "$WORKERS" = 1 ]; then
    echo "==> remote-access workers"
    UNITS="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
    mkdir -p "$UNITS"
    install -m 644 "$REPO"/remote-workers/*.service "$REPO"/remote-workers/*.timer "$UNITS/"
    install -m 755 "$REPO/remote-workers/codex-remote-control-watchdog" "$BIN/"
    systemctl --user daemon-reload
    echo "    units in $UNITS (installed, not switched on; running ones are left alone)"
    echo "    Turning these on lets anyone signed in to your account start work on this computer:"
    echo "      Claude (phone/web sessions):  systemctl --user enable --now claude-remote-control.service"
    echo "      Codex (watchdog keeps phone access up):  systemctl --user enable --now codex-remote-control.timer"
    echo "      Run them without a login:  loginctl enable-linger $(id -un)"
fi

echo "==> reload"
shell_pid=$(pgrep -u "$(id -u)" -x gnome-shell | head -n1 || true)
if [ -z "$shell_pid" ]; then
    echo "    GNOME Shell is not running; the widget loads at your next login."
    exit 0
fi
shell_started=$(( $(date +%s) - $(ps -o etimes= -p "$shell_pid" | tr -d ' ') ))
loader_mtime=$(stat -c %Y "$DEST/extension.js")
if [ "$loader_changed" = 0 ] && [ "$shell_started" -gt "$loader_mtime" ] && gnome-extensions info "$UUID" >/dev/null 2>&1; then
    # This shell runs the current loader: a disable/enable imports the new build.
    gnome-extensions disable "$UUID" || true
    gnome-extensions enable "$UUID"
    echo "    reloaded in place"
else
    echo "    This GNOME session started before the current loader was installed."
    echo "    Log out and back in once; after that, ./install.sh reloads the widget in place."
fi
