# 0031: A loop per kind of device, and a simulated robot in one node

**Status:** Proposed, 2026-10-15

## Context
[0021](0021-control-layer.md) made hardware a node: whatever publishes
`state` and takes `command`. Two such nodes existed, `keel-sim` and
`keel-can`, each with its own copy of the cycle loop, and a drive's habits
(letting go without commands) were written three times. Next came a
simulation worth the name: MuJoCo, with sensors and cameras, whose readings
should be as imperfect as a real robot's, and whose parts should break the
way real ones do, so that the health checks and the agent
([0030](0030-agent-tools.md)) have something to find.

A first try drove Lucky Engine (MuJoCo inside a closed, 1.7 GB editor) over
gRPC. It worked, but crashed when clients disconnected, couldn't render a
camera in lockstep, and needed an editor's MCP server to start its scene. It
was dropped for MuJoCo itself.

On a real robot, devices are independent: the joints behind a bus master, an
IMU on its own link, a camera through V4L2, each with its own driver, rate
and failures. In a simulation they share one world.

## Decision
- **Hardware stays a node per device on a robot** (0021 holds). What's
  shared is code: **a trait per kind of device** (`Joints`, `VectorSensor`,
  `Camera`) and **a loop per kind** (`JointLoop`, `SensorLoop`,
  `CameraLoop`), written as ticks that one scheduler runs on a clock. A
  driver is open, read, write; its loop brings the rates, stamps, deadlines
  and cycles ([0027](0027-same-cycle.md)).
- **A simulated robot is one node**, because its devices share a world: the
  same loops over simulated devices, on the world's clock, which steps the
  physics up to each tick (and in real time waits for the wall clock).
- **Physics and hardware are separate.** The physics (`Physics`: MuJoCo, or
  the pendulums) gives true values and knows nothing of sensors. A
  **hardware model** stands between it and the loops: a drive model (its own
  position or velocity loop, delay, torque limit, timeout), sensor effects
  (delay, bias, drift, noise, quantization, range), camera effects
  (exposure, noise, blur, dropouts), and faults, all seeded.
- **Laws are a layer of their own** (`law.rs`, `Pid`): the same code closes
  a loop in a controller node, in a joints node, or in a simulated drive.
- **Commands say what they ask for**: a position, a velocity or an effort.
  **Where a loop closes is a setting** (`--loop drive|keel`), not the
  controller's business.
- **Two more message layouts**: a vector of numbers (a sensor's reading, its
  meaning in its settings) and an image (a header, then RGB or depth
  pixels).
- **`keel-mujoco`, with MuJoCo as an optional C library**
  ([0016](0016-packaging-and-deployment.md) keeps C libraries out of what
  keel builds): a small C shim compiled against MuJoCo's release headers
  does whatever touches MuJoCo's structs, so Rust mirrors none; rendering is
  MuJoCo's classic OpenGL renderer through EGL, loaded at run time. Without
  MuJoCo, the crate builds and says how to get it.
- **A robot file** describes a simulated robot: the MJCF scene, where its
  sensors and cameras are mounted (added to the model through MuJoCo's
  model-editing API), its hardware and its faults. The scene's position
  servos become plain motors, and the drive model starts from their gains.

## Consequences
- `keel-sim` and `keel-can` are wiring over `JointLoop`; `keel-can-motor`
  and the emulated chip use the drive model. Their behaviour didn't change:
  same trajectories in real time, in lockstep and over a virtual CAN bus.
- The simulated Piper picks up a cube and puts it down, in real time with
  no missed deadline at 100 Hz, or in lockstep at 12× real time with three
  cameras rendering, 70× without (release build, RTX 2060). Two lockstep
  runs gave the same states and IMU readings, message for message; pictures
  differed by a level on a pixel or two (the GPU).
- Closing the position loops in keel instead of on the drives holds the arm
  at 100 Hz and makes it flail at 20 Hz: the setting matters, and is there
  to be tried.
- Breaking a part is a flag. The health checks found each fault tried: a
  dead drive, a frozen encoder (which the drive, closing its loop on its own
  encoder, doesn't notice), a frozen or noisy IMU, a dark or frozen camera.
  An agent asked what was wrong, with `keel-mcp`'s tools only
  (`examples/mujoco/diagnose.sh`), named the part and the fault each time,
  and nothing on a healthy arm: five out of five, with Sonnet.
- A robot and its simulation need different dataflow files: three driver
  nodes against one, so a controller's input names `joints/state` or
  `sim/state`. Naming outputs apart from their nodes would remove that.
- `keel-mujoco` links a shared library, so `keel deploy`'s static binaries
  can't carry it: it runs where MuJoCo is installed.
- The classic renderer looks like a game from long ago. MuJoCo's Filament
  renderer isn't in its release library; it, or Gaussian splats, would go
  behind the same render call.
- `keel-control` now depends on `serde` (for settings in robot files), and
  `keel-mujoco` on `serde_yaml`, both already in the workspace
  ([0010](0010-dependencies.md)).
