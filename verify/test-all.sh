#!/bin/sh
# Every test, in order, then a summary. Sandboxed and made-up data only: nothing signs in or out,
# opens a browser, changes a service or calls a network API.
#   1. collector unit tests            (cargo test)
#   2. collector end-to-end            (real binary, fake HOME / systemd / journal / Codex service)
#   3. Codex watchdog                  (fake Codex, fake databases, stand-in service)
#   4. AI Tracker app                  (open / quit script, fake GNOME Shell D-Bus)
#   5. widget end-to-end               (real GNOME Shell, headless; includes the real collector's output)
#   6. widget visual walk-through      (screenshots of every state, verify/out/)
set -u
V=$(cd "$(dirname "$0")" && pwd)
R=$(dirname "$V")
CARGO=${CARGO:-$(command -v cargo || echo "$HOME/.cargo/bin/cargo")}
EXTRA=$(mktemp -d "${TMPDIR:-/tmp}/au-test-all.XXXXXX")
trap 'rm -rf "$EXTRA"' EXIT
summary=""
step() { # name, command...
    name=$1; shift
    printf '\n=== %s\n' "$name"
    start=$(date +%s)
    if "$@"; then s=PASS; else s=FAIL; fi
    summary="$summary$(printf '%-34s %s  (%ss)' "$name" "$s" $(( $(date +%s) - start )))\n"
}
step "collector unit tests" "$CARGO" test --release --quiet --manifest-path "$R/collector/Cargo.toml"
step "collector build" "$CARGO" build --release --quiet --manifest-path "$R/collector/Cargo.toml"
step "collector end-to-end" python3 -I "$V/e2e/collector_e2e.py" --out "$EXTRA"
step "Codex watchdog" bash "$V/watchdog-test.sh"
step "AI Tracker app" bash "$V/app-test.sh"
step "widget end-to-end" sh "$V/e2e.sh" "$EXTRA"
visual() { sh "$V/run.sh" | tee "$EXTRA/visual.txt"; grep -q '^result: ok' "$EXTRA/visual.txt" && ! grep -q '^FAIL' "$V/out/checks.txt"; }
step "widget visual walk-through" visual
printf '\n=== summary\n%b' "$summary"
case $summary in *FAIL*) exit 1 ;; esac
