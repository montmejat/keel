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
#   openarm-dead,        OpenArm (examples/openarm.yml) against drydock, with a
#   openarm-weak,        motor broken 4 s in: the elbow's dead or weak, or
#   openarm-freeze,      the first joint's encoder frozen. Did the answer name
#   openarm-healthy      the joint, and the kind of fault? Healthy: did it
#                        find nothing? Needs drydock on PATH (or DRYDOCK=…),
#                        its repository beside this one (or DRYDOCK_ROBOT=…
#                        the robot file), and vcan0.
#
# The agent is `claude -p`, so it runs on whatever account `claude` is logged
# in to. Set MODEL to change it (default: sonnet).
set -euo pipefail
cd "$(dirname "$0")/../.."

scenario=${1:?usage: run.sh slow|stall|crash|stall-once|openarm-dead|openarm-weak|openarm-freeze|openarm-healthy}
model=${MODEL:-sonnet}
keel=./target/release/keel
# What the answer must name (`who`), and its fault; for OpenArm, what drydock
# breaks. The robot's joints are told by number and name in the health log.
who='\bfilter\b'
dataflow=examples/agent-$scenario.yml
elbow='joint ?4\b|openarm_joint4|elbow'
case $scenario in
  slow)  fault='slow|behind|processing|latency|too long|lag' ;;
  stall|stall-once) fault='stall|stuck|hung|hang|stopped|frozen|not (consuming|processing|forwarding|sending)' ;;
  crash) fault='crash|restart|exit|loop' ;;
  openarm-dead) who=$elbow; brk='{ joint: openarm_joint4, fault: dead, at: 4 s }'
    fault='dead|no torque|zero torque|0(\.0)? N ?m|not (giving|producing|applying|delivering) (any )?torque|limp|unpowered|lost power|(motor|drive) (has )?(failed|failure|fault)' ;;
  openarm-weak) who=$elbow; brk='{ joint: openarm_joint4, fault: weak, at: 4 s }'
    fault='weak|fifth|20 ?%|fraction|less torque|insufficient|under-?powered|reduced torque|not enough torque|sag' ;;
  openarm-freeze) who='joint ?1\b|openarm_joint1'; brk='{ joint: openarm_joint1, fault: freeze, at: 4 s }'
    fault='frozen|freez|stuck|not (changing|updating|moving)|unchanged|encoder|sensor|reading' ;;
  openarm-healthy) brk= ;;
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

trap 'kill $(jobs -p) 2>/dev/null || true; $keel stop >/dev/null 2>&1 || true; rm -rf "$tmp"' EXIT
task="Which node has the problem, what is wrong with it, and what is your evidence? Be brief."
intro="A keel dataflow is running on this machine and something in it is unhealthy."
if [[ $scenario == openarm-* ]]; then
  # The robot: drydock, breaking a motor 4 s in (or not). The question doesn't
  # say anything is wrong.
  robot=${DRYDOCK_ROBOT:-../drydock/examples/openarm/robot.yml}
  # What drydock breaks, as a scenario (none for a healthy arm).
  breaks=()
  if [[ -n $brk ]]; then
    echo "faults: [$brk]" > "$tmp/faults.scenario.yml"
    breaks=(--scenario "$tmp/faults.scenario.yml")
  fi
  ${DRYDOCK:-drydock} "$robot" "${breaks[@]}" > "$tmp/drydock.log" 2>&1 &
  dataflow=examples/openarm.yml
  intro="A keel dataflow drives a robot arm on this machine."
  task="Is anything wrong with it? If so: which part, what is wrong, and what is your evidence. Be brief."
  sleep 1
fi

# The agent sees the dataflow's path in `status`, so it must not name the fault.
sed -e "s#\.\./target/debug#$PWD/target/release#" -e "s#\.\./target#$PWD/target#" "$dataflow" > "$tmp/robot.yml"
$keel run "$tmp/robot.yml" > "$tmp/dataflow.log" 2>&1 &
sleep 12    # the fault happens after 4 s
if [[ $scenario == stall-once ]]; then
  task="Which node has the problem, and what is wrong with it? Fix it if you can: you may ask for a node to be restarted, which a person has to approve. Then check that it worked. Say what you found, what you did, and what you saw afterwards. Be brief."
  approver &
fi

echo "== $scenario: asking $model" >&2
answer=$(claude -p --model "$model" --mcp-config "$mcp" --strict-mcp-config \
  --allowedTools 'mcp__keel__*' --disallowedTools Bash,Read,Write,Edit,Glob,Grep <<PROMPT
$intro
Use the keel tools only. $task
PROMPT
)
echo "$answer"
echo

verdict=0
if [[ $scenario == openarm-healthy ]]; then
  # Nothing to find: it must say so, and not name a broken joint.
  if grep -qiE "no(thing)? (problem|fault|issue|wrong)|nothing (is |appears |seems )?wrong|healthy|working (normally|as expected|correctly|fine)|all (8 |eight )?joints follow" <<<"$answer" \
     && ! grep -qiE "(joint ?[0-9]|openarm_joint[0-9]|gripper)[^.]*(dead|frozen|weak|fail|broken)" <<<"$answer"; then
    echo "== diagnosis: PASS (found nothing wrong)"
  else
    echo "== diagnosis: FAIL (expected it to find nothing wrong)"; verdict=1
  fi
elif grep -qiE "$who" <<<"$answer" && grep -qiE "$fault" <<<"$answer"; then
  echo "== diagnosis: PASS (named /$who/ and its fault)"
else
  echo "== diagnosis: FAIL (expected /$who/, and /$fault/)"; verdict=1
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
