# 0036: Policies are ONNX networks, run by tract in their own node

**Status:** Accepted, 2026-10-26

## Context
The Microduck walks by a policy trained in simulation: Pollen publishes
them (Apache-2.0) as ONNX files, small networks (61 inputs, 512-256-128,
14 outputs; Gemm and Elu), run at 50 Hz. The ONNX file carries what it was
trained with: joint names, the default pose, the action scale, the names
of the observation's parts.

keel builds the middleware by hand and takes crates at the edges
([0010](0010-dependencies.md)): a network runtime is an edge.

tract (Sonos, pure Rust, MIT/Apache) runs both of Pollen's walking
policies, measured on the laptop (tract-onnx 0.23.9, release, 10,000 runs
each): 18 to 31 µs mean, under 1 ms at worst, against a 20 ms tick. It
reads the ONNX metadata. It costs 97 crates and a 24 MB binary (stripped),
against keel's six direct dependencies today. A hand-written runner for
these networks would be a couple of hundred lines and an ONNX reader; it
would stop at the first policy with another layer.

## Decision
- **tract, in its own crate and node only: `keel-policy`.** No other crate
  depends on it, so keel's daemon, CLI and other nodes stay as small as they
  are.
- **The node takes `state` and `imu`, and publishes `command`** (positions,
  [0034](0034-dynamixel-servos.md)), once per state, at the joints' rate.
- **The observation is built as the policy's metadata says**, from its
  `observation_names`, for the parts keel knows: the gyro, gravity in the
  body's frame, joint positions from the default pose, joint velocities,
  the last action, and the commands (walk, head, body), which the node
  takes as flags for now. A policy asking for a part keel doesn't know is
  refused at start, with its name.
- **The action is a target**: the default pose plus the action scale
  times the output, as Pollen's runtime applies it, through the same
  low-pass filter (Pollen's runtime says it must match training).

## Consequences
- The Microduck stands and walks from a dataflow: `keel-dynamixel` and
  `keel-policy`, against drydock, then the robot.
- tract is noted in [0010](0010-dependencies.md)'s Consequences.
- A heavier network (a VLA, a camera's) still belongs outside keel, in its
  own process; this is for policies that fit a 50 Hz tick on the robot's
  own CPU.
