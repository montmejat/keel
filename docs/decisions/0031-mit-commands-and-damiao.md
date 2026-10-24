# 0031: Commands are MIT commands, and keel-can speaks Damiao's motors

**Status:** Proposed, 2026-10-24

## Context
[0021](0021-control-layer.md) made the CAN frames up, "there's no drive to
follow yet", and a `Command` only a torque. Both were placeholders.
docs/control.md listed what replaces them: commands the drives take
(position, velocity, stiffness, damping and torque: the MIT mode of
quasi-direct-drive motors), over CAN FD.

A drive to follow now exists, and a way to run it without one: Damiao's
motors (DM-J4310, J4340, J8009…), as on OpenArm, and drydock, a separate
project that plays them behind a virtual CAN bus with MuJoCo moving the
robot, frame for frame and as fast as a real bus carries frames.

The first try at a simulation (the `archive/mujoco-sim` tag) put MuJoCo in
keel, as a node. It was retired: it brought a C library into keel
([0016](0016-packaging-and-deployment.md)), needed its own dataflow files,
and left the bus master untested. A simulation behind the bus has none of
those problems.

## Decision
- **`Command` is an MIT command**: per joint, a target position and
  velocity, two gains and a torque, five `f64` (40 bytes), asking for
  `kp (position − θ) + kd (velocity − θ̇) + effort`. A plain torque is
  `kp = kd = 0` (`Command::effort`), which is what `keel-pid` sends. Who
  closes the loop is whoever drives the joint: a Damiao motor many times a
  cycle, `keel-sim` every step, `keel-can` once a cycle for drives that only
  take a torque (the made-up ones). A controller doesn't know which.
- **`keel-can --protocol damiao`.** At start, each motor listed in
  `--motors` (its id and the id it answers on) is put in MIT mode, its
  ranges are read from its registers (PMAX, VMAX, TMAX), and it's enabled;
  at stop, disabled. Reading the ranges, rather than a table by model,
  because they're per-motor settings: OpenArm's own driver and Damiao's
  reference driver already disagree on a DM4340's.
- **One round of frames per cycle**, for either protocol: the newest
  command, as soon as the controller answers or at the deadline, the
  previous one again if none came. Damiao's motors speak only when spoken
  to, so each round's answers are the next state; before the first command,
  the round asks for feedback without commanding anything.
- **Such a state is stamped with when its round went out.** The motors
  answer as each command lands, within the round's time on the bus, not
  when keel-can reads them at the next tick.
- **CAN FD** (`--fd true`): frames of up to 64 bytes, the data at the faster
  bit rate. Damiao's frames are 8 bytes either way; CAN FD makes them
  shorter on the wire, and that is what lets a 1 kHz loop fit.
- **keel has its own copy of Damiao's frames** (`keel_control::damiao`, a
  few dozen lines of packing), not a dependency on drydock: a robot's stack
  shouldn't depend on its simulator.

## Consequences
- OpenArm v1 (seven joints and a gripper) runs from a dataflow
  (`examples/openarm.yml`): keel-can, and a controller holding a pose,
  swinging a joint and working the gripper, against drydock on `vcan0`. On a
  real OpenArm it's `--interface can0`.
- Measured on the laptop, no real-time settings, against drydock:
  - 500 Hz: state to command 15–40 µs end to end; 3 commands in 500 missed
    the 300 µs deadline. drydock has the bus 41% busy.
  - 1 kHz: 83% busy (the frame formats say 82%). Each round's answers
    finish crossing just before the next tick; about 5% of cycles have no
    whole state to publish.
  - On classic CAN, 500 Hz takes 96% of the bus, and 1 kHz doesn't fit.
- `keel-can-motor` and `keel-sim` stay, as the quick way to try a loop
  without drydock.
- Recordings of commands from before this decision don't read as commands
  any more: the layout grew from 8 bytes to 40.
- Still a cycle's delay between a command and the state it brings: the
  state published at a tick is what the previous round's answers said.
  A round sent just after the tick, answers back well before the next, and
  the controller's command sent as a second round would shorten it, at
  twice the bus load.
