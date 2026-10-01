# 0021: Control is a layer above keel, and hardware is a node

**Status:** Proposed, 2026-10-01

## Context
keel moves bytes between processes and looks after them. What a robot does
with that is control: read joints, compute, command them, a thousand times
a second, over a fieldbus (CAN, EtherCAT) or against a simulator (MuJoCo).
The question of 2026-10-01: is that core software, or a separate thing?
Real robots are in scope ([0001](0001-learning-project-whole-stack.md), as
amended), and [0003](0003-raw-byte-payloads.md) says payloads are raw
bytes.

## Decision
- **A separate crate, `keel-control`, above the node API.** It depends on
  `keel` and `libc`; nothing in keel depends on it. It could live in another
  repository; it's in the workspace to be built and tested with the rest.
- **Hardware is a node, not a trait.** Whatever publishes `state` and takes
  `command` is the joints. There's no hardware-interface trait and no
  plugins: the dataflow file is the interface, and choosing between a
  simulation and a bus is a change to it (which `keel diff` shows, and a
  branch can carry, [0020](0020-branches.md)).
- **Two message layouts**, owned by this crate, not by keel: per joint, a
  `State` (position, velocity, as `f64`) and a `Command` (effort), back to
  back, little-endian. keel still sees bytes.
- **Nodes:**
  - `keel-pid`: a command for each state. Event-driven, so the command is
    part of the state's trace.
  - `keel-sim`: pendulums stepped in real time at a fixed rate.
  - `keel-can`: a cyclic master for joints on a CAN bus, through SocketCAN
    (a raw socket, via `libc`).
  - `keel-can-motor`: the same pendulums behind CAN frames, standing in for
    a drive, so the bus can be tried on a virtual interface.
- **The CAN frames are made up** (a command frame and a state frame per
  joint): there's no drive to follow yet. A real one (CANopen CiA 402, or a
  vendor's) replaces that part of `keel-can`.
- **A node that loses its bus fails**, and the dataflow's restart policy
  ([0019](0019-lifecycle.md)) brings it back: no retry logic of its own.

## Consequences
- Measured on the laptop, no real-time settings: state to controller 35 µs
  (p50), and the command waits for the joints' next cycle, so about one
  period (1 ms) from state to applied command, on the simulation and on the
  virtual bus alike.
- The loops aren't synchronised: the master, the drive and the simulation
  each tick on their own clock. Fine at 1 kHz for a PID; a tighter loop
  wants the bus cycle to drive the controller.
- The simulation runs in real time only. Faster than real time, or
  stepping, needs a clock nodes can be given: a change to keel itself.
- Two layouts is not a type system. More message kinds will raise 0003
  again.
- Not done: EtherCAT (a master over raw Ethernet frames is a large piece by
  hand; the `ethercrab` crate is the alternative), MuJoCo (a C library,
  which [0016](0016-packaging-and-deployment.md) keeps out of what keel
  builds, so it would be an optional node), several joints tried for real
  (the code takes `--joints`, only one was run).
- Being a separate crate isn't about scope (control is wanted) but about
  direction: keel's core stays ignorant of joints and buses, so it stays
  small, and what control needs from it (a clock, typed messages) shows up
  as a gap in the node API rather than as a special case in the daemon.
