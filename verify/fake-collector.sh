#!/bin/sh
# Stand-in for ~/.local/bin/agent-usage inside the sandbox. A scenario is either <name>.json (one
# snapshot, printed after $delay) or <name>.stream (lines "<delay-seconds> <json>", printed one by
# one, like `agent-usage --stream`).
d="$HOME/scenarios"
name=$(cat "$d/current" 2>/dev/null || echo healthy)
delay=$(cat "$d/delay" 2>/dev/null || echo 0.3)
echo "start $name $*" >> "$d/runs.log"
if [ -f "$d/$name.stream" ]; then
    while IFS= read -r line; do
        sleep "${line%% *}"
        printf '%s\n' "${line#* }"
    done < "$d/$name.stream"
else
    sleep "$delay"
    cat "$d/$name.json"
fi
