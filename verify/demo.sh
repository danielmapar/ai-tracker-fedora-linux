#!/bin/sh
# Record docs/demo.gif: the widget in run.sh's sandboxed headless GNOME Shell with made-up data,
# driven by a virtual mouse (demo-extension.js), then assembled by make-gif.py (Pillow + ffmpeg).
set -eu
V=$(cd "$(dirname "$0")" && pwd)
OUT=$V/out-demo
rm -rf "$OUT"
mkdir -p "$OUT/frames"
AU_HARNESS="$V/demo-extension.js" AU_OUT_DIR="$OUT" AU_SNAPSHOT=demo-before AU_BACKGROUND='#16161b' \
    sh "$V/run.sh" 14 || true   # its exit status is not the verdict; DONE is
[ "$(cat "$OUT/DONE" 2>/dev/null)" = ok ] || { echo "recording failed (see $OUT/shell.log)" >&2; exit 1; }
python3 -I "$V/make-gif.py" "$OUT" "$(dirname "$V")/docs/demo.gif"
