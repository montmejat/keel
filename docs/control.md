# Control

`keel-control` is the layer that turns keel into a robot's control stack:
joints, a controller, the buses behind them, and a simulated robot. It's a crate
above the node API, not part of keel ([0021](decisions/0021-control-layer.md)):
everything here is built with what any user of keel gets.

## The layers

```
 controller          keel-pid, arm-routine, or yours: sees `state`, sends `command`
 ─────────────────── keel: state ⇄ command, stamped, in shared memory ───
 devices             keel-can (joints on a bus)  |  keel-mujoco (simulated)
 ─────────────────── the wire: can0 ──────────── |
 drives              real drives                 |  the drive model, in MuJoCo
```

**The dataflow is the hardware interface.** Whatever publishes `state` and
takes `command` is the joints. The controller can't tell a simulation from a
bus, or a live run from a replay or from a simulation in lockstep, and
nothing above the joints should try to: swapping one for the other is a
change to the dataflow file.

**On a robot, a node per device**: the joints behind their bus, an IMU on
its link, a camera, each with its own driver, failing and restarting on its
own. **A simulated robot is one node**, because its devices share a world:
the same loops, over simulated devices ([0031](decisions/0031-devices-and-simulation.md)).

The one simulation is MuJoCo's: no stand-in drives on a virtual bus, no
pretend chip ([0031](decisions/0031-devices-and-simulation.md)).

## Inside keel-control

In layers, each using only those below it:

| layer | what | where |
|---|---|---|
| nodes | wiring only | `src/bin`, `keel-mujoco` |
| loops | `JointLoop`, `SensorLoop`, `CameraLoop`: rates, stamps, deadlines; one scheduler runs them on the wall clock or a simulation's | `schedule.rs`, `cycle.rs` |
| devices | a trait per kind: `Joints`, `VectorSensor`, `Camera`; real drivers (`CanJoints`) and simulated ones (`SimJoints`, `SimSensor`, `SimCamera`) | `device.rs`, `can.rs`, `sim.rs` |
| hardware model | a drive; a sensor's and a camera's imperfections and faults | `drive.rs`, `model.rs` |
| laws | `Pid`, for positions and velocities: pure maths | `law.rs` |
| messages | `State`, `Command`, a vector, an `Image` | `lib.rs` |

So `keel-can` is the joints' loop over a bus, and `keel-mujoco` runs the
same loop over simulated joints: a few lines of wiring each. A new real device is one trait
implementation (open, read, write); its loop comes with it.

A simulated device is a physics engine's truth seen through a hardware
model. The physics (`Physics`: MuJoCo) knows nothing of
sensors. A drive model stands between a command and the torque, with the
drive's own loop, a delay, a torque limit, letting go without commands, and
faults; each sensor reading goes through delay, bias, drift, noise,
quantization and range; each picture through exposure, noise, blur and
dropouts. All seeded: two runs of the simulated Piper in lockstep gave the
same joint states and IMU readings, message for message (7473 and 14946 of
them); its pictures differed by a level, on a pixel or two, now and then:
the GPU.

## Control modes

A command says what it asks for: a **position**, a **velocity** or an
**effort**. Where a position or velocity loop closes is a setting of the
joints, not of the controller:

- `--loop drive` (default): commands go to the drives as they are, and the
  drive closes the loop: a real drive's firmware, or the drive model, at
  the simulation's rate.
- `--loop keel`: the joints' loop closes it, every tick, with the same
  `Pid`, and sends the drives efforts.

The controller doesn't change. The rate does matter: the simulated Piper
holds its poses with its loops in keel at 100 Hz, and flails at 20 Hz,
where the drives, closing them at 500 Hz, still hold it. A controller node
like `keel-pid` stays the place for laws that need more than one joint.

## Messages

Payloads are raw bytes to keel ([0003](decisions/0003-raw-byte-payloads.md));
this crate gives four of them a layout, little-endian:

| message | layout | bytes |
|---|---|---:|
| `State` | per joint: position (rad), velocity (rad/s), as `f64` | 16 a joint |
| `Command` | per joint: a value as `f64`, its mode in a byte (effort, position, velocity), padding | 16 a joint |
| vector | a sensor's numbers as `f64`s, their meaning in the sensor's settings | 8 a number |
| `Image` | width, height, format (RGB8, or depth as `f32` metres), a spare `u32`, then the pixels | 16 + pixels |

```rust
for state in State::read(&data) { /* state.position, state.velocity */ }
node.send_with("command", n * Command::LEN, |p| Command::write(&commands, p))?;
```

Every message also carries, in keel's header, a **stamp**: the moment of the
world it describes ([0028](decisions/0028-stamps.md)). A state's stamp is
when its joints were read, or the simulation's time; a command inherits the
stamp of the state it answers.

## Nodes

| node | what it is | flags |
|---|---|---|
| `keel-can` | a cyclic bus master over SocketCAN | `--interface can0 --joints 1 --hz 1000 --cycle same\|next --deadline-us 300 --loop drive\|keel --spin-us 0` |
| `keel-mujoco` | a simulated robot: joints, sensors, cameras ([below](#a-simulated-robot)) | `--robot examples/mujoco/piper.yml --cycle same\|next\|lockstep --break joint2:dead@10 --loop drive\|keel --hz 0` |
| `keel-pid` | a PID per joint, a command for each state | `--target 1.0 --kp 40 --ki 40 --kd 4 --limit 10 --spin-us 0 --period-us 0` |

The CAN frames are made up (a command frame and a state frame per joint):
a real drive's protocol replaces them in `keel-can`.

## On a microcontroller

`keel-micro` is the node API for a chip: `no_std`, no allocation, no
dependencies, over any byte link (a UART)
([0025](decisions/0025-microcontroller-nodes.md)). On the machine it's
plugged into, `keel-serial` stands in for it as an ordinary node, so the
chip shows up in `keel top`, traces and recordings like the rest:

```rust
let mut node: keel_micro::Node<Uart, 64> = keel_micro::Node::new(uart);
loop {
    while let Some((COMMAND, payload)) = node.poll() { /* drive the motor */ }
    node.send(STATE, &encoder.to_le_bytes());
}
```

It builds for a Cortex-M (`cargo build -p keel-micro --target
thumbv7em-none-eabihf`) and hasn't run on a real chip yet.

## A cycle

A bus master or simulation ticks once a period. **By default it closes the
loop within the cycle** ([0027](decisions/0027-same-cycle.md)):

```
tick k                                                     tick k+1
  │ publish state(k)                                         │ publish state(k+1)
  │   wait for cmd(k), up to the deadline (300 µs) ┐         │   ...
  │──┐                                             │         │
  │  └→ controller: state(k) → cmd(k)  (~10 µs) ───┘         │
  │   apply cmd(k): the simulation steps, the master sends   │
  │   sleep ................................................→│
```

- `--cycle same` (default): as above. The answer is recognised by its trace
  context: the controller sent it while holding the state, so the state is
  its parent. If it's late, the joints keep the previous command, the late
  one is used at the next tick, and the misses are logged once a second.
- `--cycle next`: commands are taken at the next tick, a constant
  one-period delay, for controllers that can't answer within the deadline.
- `--cycle lockstep` (simulations only): no clock and no deadline; the
  simulation steps as soon as the controller answers, faster or slower
  than real time. The controller can't tell, because it takes its time from
  the stamps.

## Time

### Stamps
A controller takes its time steps from the states' stamps, never from the
clock: `dt` is the gap between two stamps. That's what makes it behave the
same against joints, a simulation in lockstep, or a replay at any speed.
Recordings keep the stamps, and replays hand them on.

### Phases
Periodic loops tick on a grid: multiples of their period on the machine's
monotonic clock ([0029](decisions/0029-phases.md)). So loops of one period
tick together whenever they started, and a 20 ms loop ticks with every 20th
tick of a 1 ms one. `phase_us` in the dataflow moves a node along its
period:

```yaml
  - id: imu
    path: ../target/debug/my-imu
    phase_us: 900          # read 100 µs before the bus master's tick
```

Two bus masters with the same period are in phase without being told. Phases
hold within a machine: machines' clocks aren't aligned to the microsecond.

### Waiting
A node waits for its inputs in one of three ways:

| | how | CPU | one hop (laptop, p50) |
|---|---|---:|---:|
| sleep | futex (default) | ~0% | ~5 µs |
| spin | `Node::set_spin`, `--spin-us` | 100% of a core | ~0.3–0.5 µs |
| spin around | `Node::set_spin_around`, `--spin-us` with `--period-us` | a few % | ~1 µs |

Spinning around watches only from `spin` before each expected arrival to
`spin` after it; set the node's `phase_us` to when its inputs are sent. A
bus master waiting for its answer spins only within its deadline window.
Spinning wants a CPU of its own: `rt: { cpus: [3] }`, ideally isolated.

## Writing a controller

```rust
use keel::{Event, Node};
use keel_control::{Command, State};

fn main() -> std::io::Result<()> {
    let mut node = Node::from_env()?;
    let (mut last, mut commands) = (0, Vec::new());
    while let Event::Input { data, .. } = node.next_event()? {
        let dt = if last == 0 { 0.0 } else { (data.stamp_ns() - last) as f64 / 1e9 };
        last = data.stamp_ns();
        commands.clear();
        commands.extend(State::read(&data).map(|s| Command::effort(law(s, dt))));
        // Sent while `data` is held: the command answers this state, within
        // its cycle, and carries its stamp.
        node.send_with("command", commands.len() * Command::LEN, |p| Command::write(&commands, p))?;
    }
    Ok(())
}
```

Three rules:
1. **Send while holding the state** (or with `send_caused_by`). A command
   sent after the sample is dropped answers nothing: the joints wait for
   their deadline every cycle. Use `--cycle next` with such a controller.
2. **Take time from stamps**, not from the clock.
3. **Know nothing of what's behind the joints.** If a controller needs to
   know, the interface is missing something: add it to the messages.

## Measured

On a laptop (Core Ultra 7 155H, powersave, no isolation), 1 kHz, from a
state published to its command applied (`keel trace`, end to end):

| | |
|---|---:|
| next cycle (before 0027) | ~1000 µs |
| same cycle, sleeping | ~20 µs |
| same cycle, the simulation spinning in its window | ~15 µs |
| same cycle, the controller spinning around its states (6% of a core) | ~3–4 µs |
| same cycle, both spinning, each pinned to a core | ~1.3 µs |

On a virtual CAN bus, a command reached `keel-can` 4–5 µs after it was
sent, where it used to wait 950 µs for the next tick. In lockstep, the
simulation ran 1301 simulated seconds in 6 s, on the same trajectory as in
real time. (Those were measured with the pendulum simulation and stand-in
CAN drive that 0031 retired.)

## Examples

The examples are the simulated robot's ([below](#a-simulated-robot)). On a
robot, `keel-pid` or your controller takes `keel-can`'s `state`:

```yaml
nodes:
  - id: joints
    path: ../target/debug/keel-can
    args: ["--interface", "can0", "--joints", "1", "--hz", "1000"]
    inputs:
      command: { source: controller/command, keep: latest }
  - id: controller
    path: ../target/debug/keel-pid
    args: ["--target", "1.0"]
    inputs:
      state: { source: joints/state, keep: latest }
```

```sh
keel trace                                 # state → command, end to end
```

## A simulated robot

`keel-mujoco` is a robot in [MuJoCo](https://mujoco.org): its joints, and
the sensors and cameras mounted on it, as simulated devices in one node.
Its robot file says what it is (`examples/mujoco/piper.yml`):

- **the scene**, an MJCF file: here MuJoCo Menagerie's AgileX Piper on a
  table, with a cube to pick up;
- **mounts**: an IMU and a force/torque sensor in the wrist, a colour
  camera and a depth camera beside the gripper, each on a body of the
  scene, each an output of the node at its own rate;
- **the hardware**: every drive (starting from the scene's own servo gains:
  the scene's position servos become plain motors, and keel's drive model
  takes over), every encoder, each sensor's and camera's imperfections;
- **faults**: in the file, or `--break joint2:dead@10,wrist:dark@20` (or
  `KEEL_BREAK` in keel's environment): a drive dead or weak, an encoder or a
  sensor frozen or noisy, a camera frozen or dark.

MuJoCo is a C library: `examples/mujoco/setup.sh` fetches its release (and
the Piper), and `keel-mujoco` builds with it from then on, through a small C
shim; without it, it builds and says how to get it. Its pictures come from
MuJoCo's classic OpenGL renderer, headless through EGL.

```sh
examples/mujoco/setup.sh && cargo build
keel run examples/mujoco-arm.yml           # pick and place, in real time
keel logs health                           # "health ok: 100 states/s, worst tracking joint5 0.211"
ls /tmp/keel-frames                        # wrist.ppm, wrist_depth.pgm, overview.ppm
keel run examples/mujoco-lockstep.yml      # the same, as fast as it goes
KEEL_BREAK=joint3:freeze@8 keel run examples/mujoco-lockstep.yml
keel logs health                           # "PROBLEM joint3 reads exactly -1.3212, not a bit of change..."
examples/mujoco/diagnose.sh joint2:dead    # an agent, with keel-mcp's tools only, says what broke
```

Measured on the laptop (RTX 2060), release build, lockstep: 12× real time
with the three cameras (30 pictures a second, five of them 640×480), 70×
without. In real time, the joints' loop at 100 Hz missed no deadline. The
health checks found each fault tried (a dead drive, a frozen encoder, a
frozen or noisy IMU, a dark or frozen camera) and nothing on a healthy run.
Asked what was wrong with the robot, with `keel-mcp`'s tools only, an agent
(Sonnet, `diagnose.sh`) named the part and its fault for a dead drive, a
frozen encoder, a noisy IMU and a covered camera, and found nothing wrong
with a healthy arm: five out of five.

## Not yet

- **Commands the drives take**: stiffness and damping alongside position
  and torque (the MIT mode of quasi-direct-drive motors), and their
  protocol over CAN-FD, in place of the made-up frames.
- **A fresh state from request/response drives**: drives that answer each
  command with their state report one taken a cycle ago. A state request
  just before the tick (a phase) fixes it.
- **Several buses, one controller**: two arms, two masters in phase, and a
  controller that waits for both states of a cycle.
- **Real sensor drivers**: an IMU's and a camera's (V4L2), as nodes of their
  own over `SensorLoop` and `CameraLoop`.
- **Prettier pictures**: MuJoCo's Filament renderer isn't in its release
  library; Gaussian splats would go behind the same render call.
- **The gripper's health**: it's a slide joint, in metres, and the checks'
  thresholds are in radians: a dead gripper goes unnoticed.
- **Slow loops**: a policy every Nth cycle, and an interpolator playing its
  actions back by their stamps.
- **The age of a command** in `keel trace`: from its stamp to its sending.
