#!/usr/bin/env bash
# Starts a scenario (slow, stall or crash), lets its fault happen, asks an
# agent what is wrong with the tools of keel-mcp only, and grades the answer:
# did it name the filter, and the kind of fault?
#
#   cargo build --release && examples/agent/run.sh stall
#
# The agent is `claude -p`, so it runs on whatever account `claude` is logged
# in to. Set MODEL to change it (default: sonnet).
set -euo pipefail
cd "$(dirname "$0")/../.."

scenario=${1:?usage: run.sh slow|stall|crash}
model=${MODEL:-sonnet}
case $scenario in
  slow)  fault='slow|behind|processing|latency|too long|lag' ;;
  stall) fault='stall|stuck|hung|hang|stopped|frozen|not (consuming|processing|forwarding|sending)' ;;
  crash) fault='crash|restart|exit|loop' ;;
  *) echo "no scenario $scenario" >&2; exit 2 ;;
esac

tmp=$(mktemp -d)
mcp=$tmp/mcp.json
echo "{\"mcpServers\":{\"keel\":{\"command\":\"$PWD/target/release/keel-mcp\"}}}" > "$mcp"

./target/release/keel run "examples/agent-$scenario.yml" > "$tmp/dataflow.log" 2>&1 &
trap './target/release/keel stop >/dev/null 2>&1 || true; wait; rm -rf "$tmp"' EXIT
sleep 12    # the fault happens after 4 s

echo "== $scenario: asking $model" >&2
answer=$(claude -p --model "$model" --mcp-config "$mcp" --strict-mcp-config \
  --allowedTools 'mcp__keel__*' --disallowedTools Bash,Read,Write,Edit,Glob,Grep <<'PROMPT'
A keel dataflow is running on this machine and something in it is unhealthy.
Use the keel tools only. Which node has the problem, what is wrong with it, and
what is your evidence? Be brief.
PROMPT
)
echo "$answer"
echo
if grep -qiw filter <<<"$answer" && grep -qiE "$fault" <<<"$answer"; then
  echo "== PASS: named the filter and its fault"
else
  echo "== FAIL: expected the filter, and /$fault/"
  exit 1
fi
