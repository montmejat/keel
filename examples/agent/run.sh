#!/usr/bin/env bash
# Starts a scenario, lets its fault happen, asks an agent about it with the
# tools of keel-mcp only, and grades what came of it.
#
#   cargo build --release && examples/agent/run.sh stall
#
#   slow, stall, crash   diagnose: did the answer name the filter, and the
#                        kind of fault?
#   stall-once           diagnose and fix: the agent may ask to restart a
#                        node. A stand-in for the person approves restarts
#                        and denies anything else. Graded on the answer, and
#                        on data flowing again afterwards.
#
# The agent is `claude -p`, so it runs on whatever account `claude` is logged
# in to. Set MODEL to change it (default: sonnet).
set -euo pipefail
cd "$(dirname "$0")/../.."

scenario=${1:?usage: run.sh slow|stall|crash|stall-once}
model=${MODEL:-sonnet}
keel=./target/release/keel
case $scenario in
  slow)  fault='slow|behind|processing|latency|too long|lag' ;;
  stall|stall-once) fault='stall|stuck|hung|hang|stopped|frozen|not (consuming|processing|forwarding|sending)' ;;
  crash) fault='crash|restart|exit|loop' ;;
  *) echo "no scenario $scenario" >&2; exit 2 ;;
esac

tmp=$(mktemp -d)
mcp=$tmp/mcp.json
echo "{\"mcpServers\":{\"keel\":{\"command\":\"$PWD/target/release/keel-mcp\"}}}" > "$mcp"
rm -f /tmp/keel-agent-stall-once

# The person: approves a restart, denies anything else, once a second.
approver() {
  while sleep 1; do
    $keel approve 2>/dev/null | awk '/\[waiting\]/ { print $1 "\t" $0 }' | while IFS=$'\t' read -r id line; do
      if [[ $line == *"restart node"* ]]; then $keel approve "$id" >/dev/null; else $keel deny "$id" >/dev/null; fi
    done
  done
}

# The agent sees the dataflow's path in `status`, so it must not name the fault.
sed "s#\.\./target#$PWD/target#" "examples/agent-$scenario.yml" > "$tmp/robot.yml"
$keel run "$tmp/robot.yml" > "$tmp/dataflow.log" 2>&1 &
trap 'kill $(jobs -p) 2>/dev/null || true; $keel stop >/dev/null 2>&1 || true; rm -rf "$tmp"' EXIT
sleep 12    # the fault happens after 4 s

task="Which node has the problem, what is wrong with it, and what is your evidence? Be brief."
if [[ $scenario == stall-once ]]; then
  task="Which node has the problem, and what is wrong with it? Fix it if you can: you may ask for a node to be restarted, which a person has to approve. Then check that it worked. Say what you found, what you did, and what you saw afterwards. Be brief."
  approver &
fi

echo "== $scenario: asking $model" >&2
answer=$(claude -p --model "$model" --mcp-config "$mcp" --strict-mcp-config \
  --allowedTools 'mcp__keel__*' --disallowedTools Bash,Read,Write,Edit,Glob,Grep <<PROMPT
A keel dataflow is running on this machine and something in it is unhealthy.
Use the keel tools only. $task
PROMPT
)
echo "$answer"
echo

verdict=0
if grep -qiw filter <<<"$answer" && grep -qiE "$fault" <<<"$answer"; then
  echo "== diagnosis: PASS (named the filter and its fault)"
else
  echo "== diagnosis: FAIL (expected the filter, and /$fault/)"; verdict=1
fi

if [[ $scenario == stall-once ]]; then
  echo "== what was asked, and decided:"; $keel approve
  # Is data reaching the sink? Two samples of the filter's output, apart.
  moved=$( { timeout 3 $keel events --interval-ms 1000 || true; } | python3 -c '
import sys, json
n = []
for line in sys.stdin:
    e = json.loads(line)
    if "status" in e:
        n += [l["messages"] for l in e["status"]["links"] if l["source"] == "filter/filtered"]
print("yes" if len(n) >= 2 and n[-1] > n[0] else "no")')
  if [[ $moved == yes ]]; then echo "== recovery: PASS (the filter is sending again)"
  else echo "== recovery: FAIL (nothing is coming out of the filter)"; verdict=1; fi
fi
exit $verdict
