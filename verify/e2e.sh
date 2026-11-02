#!/bin/sh
# Widget end-to-end tests: verify/e2e/widget-e2e.js in a sandboxed headless GNOME Shell (see
# verify/run.sh). Clicks real buttons and checks what is on screen and which commands ran; fake
# terminals, systemctl and systemd-run only record. Never signs in or out, never opens a browser.
#   sh verify/e2e.sh [folder with extra scenarios, e.g. the real collector's output]
#   AU_E2E_ONLY='Account|Limits' sh verify/e2e.sh    # only the tests whose "group › name" matches
# Output: verify/out-e2e/ (one screenshot per test, e2e-results.json, checks.txt, shell.log).
set -u
V=$(cd "$(dirname "$0")" && pwd)
OUT=$V/out-e2e
mkdir -p "$OUT"
rm -f "$OUT"/*.png "$OUT/e2e-results.json" "$OUT/checks.txt"
AU_HARNESS="$V/e2e/widget-e2e.js" AU_OUT_DIR="$OUT" AU_TIMEOUT=600 AU_EXTRA_SCENARIOS="${1:-}" \
  sh "$V/run.sh" > "$OUT/run.txt" 2>&1
python3 -I - "$OUT" <<'PY'
import json, sys, re, collections
out = sys.argv[1]
try:
    res = json.load(open(f"{out}/e2e-results.json"))
except (OSError, ValueError):
    print("widget e2e: no results (see verify/out-e2e/run.txt and shell.log)")
    sys.exit(1)
log = open(f"{out}/shell.log", errors="replace").read()
js = [l for l in log.splitlines() if re.search(r"JS ERROR|JS WARNING|CRITICAL", l) and "NM.Object" not in l]
group = None
for r in res:
    if r["group"] != group:
        group = r["group"]
        print(f"\n{group}")
    mark = {"PASS": "✓", "FAIL": "✗", "SKIP": "–"}[r["status"]]
    print(f"  {mark} {r['name']}  ({r['ms'] / 1000:.1f} s)")
    if r["error"]:
        print("      " + r["error"].splitlines()[0])
n = collections.Counter(r["status"] for r in res)
print(f"\nJS errors in the shell log: {len(js)}")
for l in js[:10]:
    print("  " + l[:200])
print(f"widget e2e: {n['PASS']} passed, {n['FAIL']} failed, {n['SKIP']} skipped")
sys.exit(1 if n["FAIL"] or js else 0)
PY
