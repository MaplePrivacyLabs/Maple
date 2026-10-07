#!/bin/sh
# Smoke-level performance check for a Maple Agent release build.
#
# Measures what the plan asks for, on this machine, as a sanity check rather
# than a gate: cold start to an interactive window, memory at idle and after a
# task, and whether a long streamed answer stays smooth (recorded by hand,
# see the prompt below). Time to first token depends on the network, so only
# Maple's own startup is timed.
#
# Usage: perf-check.sh <maple-agent binary> [idle seconds]
# Run it with the same environment the app normally gets (MAPLE_API_URL,
# XDG_* and so on). It launches the binary with RUST_LOG=warn,maple_agent=debug,
# reads the `startup:` markers from its stderr, samples resident memory once
# the window is open and again after the idle period, then leaves the app
# running so a long task can be driven by hand. Stop the app yourself.
set -eu
binary=${1:?usage: perf-check.sh <maple-agent binary> [idle seconds]}
idle=${2:-30}
log=$(mktemp "${TMPDIR:-/tmp}/maple-perf.XXXXXX")
RUST_LOG=warn,maple_agent=debug "$binary" >"$log" 2>&1 &
pid=$!
echo "pid $pid, log $log"
for _ in $(seq 1 600); do
  if grep -q 'startup: runtime started' "$log"; then break; fi
  sleep 0.1
done
echo "--- startup markers (ms since process start)"
grep -o 'startup: .* at [0-9]* ms' "$log" | sed 's/startup: //; s/ from [^ ]* at / at /'
rss() { ps -o rss= -p "$pid" | awk '{printf "%.1f MB", $1/1024}'; }
echo "--- memory"
echo "window open: $(rss)"
sleep "$idle"
echo "after ${idle}s idle: $(rss)"
cat <<MSG
--- long answer
Now ask the app for a long streamed answer, for example:
  "Write a 1500-word essay about the history of the printing press."
Watch the transcript while it streams: it should scroll smoothly with no
visible stalls. Record "smooth" or describe the stall, then run:
  ps -o rss= -p $pid   # memory after the long task, in KB
MSG
