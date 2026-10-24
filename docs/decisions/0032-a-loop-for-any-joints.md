# 0032: A driver is what its joints need; one loop runs any of them

**Status:** Proposed, 2026-10-24

## Context
`keel-sim` and `keel-can` each had their own copy of a joints' cycle:
tick, publish the state, wait for the command within the deadline, count
the misses, say when the joints go quiet. With Damiao's motors
([0031](0031-mit-commands-and-damiao.md)), `keel-can` grew a second kind of
drive inside its binary, and the copy grew with it.

The `archive/mujoco-sim` tag had an answer, in a broader form: a trait per
kind of device (joints, numeric sensors, cameras), a loop per kind, and a
scheduler running many loops in one node, for a simulated robot inside keel.
That simulation is now drydock's, behind the bus, and keel has no sensor or
camera drivers; the joints' half is what's still needed.

## Decision
- **`Joints`: a driver is count, start, read, write, stop.** `read` gives
  every joint's state and the moment it describes, once there's a whole
  one; `write` takes the newest commands every cycle, or `None` before the
  controller has sent any; `start` and `stop` are for what drives need
  around a run (Damiao's: MIT mode, ranges, enable; disable).
- **`JointLoop`: the cycle, once**, for any `Joints`: the tick, the state
  out, the wait within the cycle (or the next cycle, or lockstep), one write
  per cycle, the log lines, and `stop` however it ends.
- **Three drivers**, in the library rather than in binaries:
  `can::CanJoints` (made-up drives, the command's law worked out once a
  cycle), `damiao::DamiaoJoints`, `plant::Pendulums`. `keel-can` and
  `keel-sim` are wiring.
- **Not brought back** from the archive: the per-mode commands (MIT commands
  cover them), the scheduler of many loops, the sensor and camera loops and
  their message layouts (nothing in keel produces them yet), and the
  simulated devices and hardware model (drydock's now).

## Consequences
- A new driver (another vendor's motors, EtherCAT) is a `Joints` and two
  lines of wiring; the cycle's behaviour, and its fixes, are shared.
- Unchanged behaviour, run end to end: the pendulum holds at 1 rad pushing
  4.13 N m, in real time and in lockstep; the made-up drives on `vcan0` the
  same; OpenArm against drydock as in 0031.
- Sensors and cameras get their own traits and loops when there's a driver
  to write; the archive shows one way.
