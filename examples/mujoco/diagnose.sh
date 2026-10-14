#!/usr/bin/env bash
# Breaks a part of the simulated Piper without saying which, lets the arm
# work a while, asks an agent what is wrong with the robot (with keel-mcp's
# tools only), and grades the answer: did it name the part and the fault?
#
#   cargo build --release && examples/mujoco/diagnose.sh joint2:dead
#
#   joint2:dead      a drive gives no torque
#   joint3:freeze    an encoder stops updating
#   imu:noisy        the IMU's readings grow noisy
#   wrist:dark       the wrist camera's lens is covered
#   healthy          nothing: it should say so
#
# The agent is `claude -p`, so it runs on whatever account `claude` is logged
# in to. Set MODEL to change it (default: sonnet).
set -euo pipefail
cd "$(dirname "$0")/../.."

scenario=${1:?usage: diagnose.sh joint2:dead|joint3:freeze|imu:noisy|wrist:dark|healthy}
model=${MODEL:-sonnet}
keel=./target/release/keel
case $scenario in
  joint2:dead) part='joint ?2'; fault='drive|motor|torque|not reaching|not following|stuck|blocked' ;;
  joint3:freeze) part='joint ?3'; fault='encoder|sensor|frozen|not updating|stopped updating' ;;
  imu:noisy) part='imu'; fault='nois' ;;
  wrist:dark) part='camera|wrist'; fault='dark|covered|bright' ;;
  healthy) part='.'; fault='no (problems?|issues?|faults?)|healthy|nothing (is )?wrong|working (normally|correctly|fine)' ;;
  *) echo "no scenario $scenario" >&2; exit 2 ;;
esac
[[ $scenario == healthy ]] && broken='' || broken=$scenario@5

tmp=$(mktemp -d)
mcp=$tmp/mcp.json
echo "{\"mcpServers\":{\"keel\":{\"command\":\"$PWD/target/release/keel-mcp\"}}}" > "$mcp"
sed "s#\.\./target/debug#$PWD/target/release#" examples/mujoco-arm.yml > "$tmp/robot.yml"
KEEL_BREAK=$broken $keel run "$tmp/robot.yml" > "$tmp/dataflow.log" 2>&1 &
trap 'kill $(jobs -p) 2>/dev/null || true; $keel stop >/dev/null 2>&1 || true; rm -rf "$tmp"' EXIT
sleep 25    # the part breaks 5 s in

echo "== $scenario: asking $model" >&2
answer=$(claude -p --model "$model" --mcp-config "$mcp" --strict-mcp-config \
  --allowedTools 'mcp__keel__*' --disallowedTools Bash,Read,Write,Edit,Glob,Grep <<PROMPT
A robot arm is running on this machine, as a keel dataflow. Use the keel tools
only. Is anything wrong with the robot? If so, which part, what is wrong with
it, and what is your evidence? Be brief.
PROMPT
)
echo "$answer"
shopt -s nocasematch
if [[ $answer =~ $part ]] && [[ $answer =~ $fault ]]; then echo "== PASS: $scenario" >&2; else echo "== FAIL: $scenario" >&2; exit 1; fi
