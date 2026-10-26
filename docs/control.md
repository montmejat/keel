# Control

`keel-control` is the layer that turns keel into a robot's control stack:
joints, a controller, the buses and simulations behind them. It's a crate
above the node API, not part of keel ([0021](decisions/0021-control-layer.md)):
everything here is built with what any user of keel gets.

## The layers

```
 controller          keel-pid, or yours: sees `state`, sends `command`
 ─────────────────── keel: state ⇄ command, stamped, in shared memory ───
 joints              keel-can (a bus master)   |  keel-sim (a simulation)
 ─────────────────── the wire: can0 / vcan0 ────────────────────────────
 drives              real drives               |  drydock (MuJoCo)
```

**The dataflow is the hardware interface.** Whatever publishes `state` and
takes `command` is the joints; there's no driver trait and no plugin. The
controller can't tell a simulation from a bus, or a live run from a replay
or from a simulation in lockstep, and nothing above the joints should try
to: swapping one for the other is a change to the dataflow file.

A simulation can also sit lower, below the wire, so the bus master and its
protocol run in simulation as they would on a robot:
[drydock](https://github.com/montmejat/drydock), a separate project, plays
Damiao's motors on a virtual bus with MuJoCo moving the robot behind them.

## Messages

Payloads are raw bytes to keel ([0003](decisions/0003-raw-byte-payloads.md));
this crate gives two of them a layout. One message carries every joint of a
bus or simulation, back to back, little-endian:

| message | per joint | bytes |
|---|---|---:|
| `State` | position (rad), velocity (rad/s), effort (N m: the torque the drive gives, as it measures it), as `f64`; then its status, two `u32`: flags (drive error, disabled, underpowered, behind, frozen) and the drive's own code | 32 |
| `Command` | position (rad), velocity (rad/s), kp (N m/rad), kd (N m s/rad), effort (N m), as `f64` | 40 |

Two more carry the whole body, one per message: an `Imu` reading
([0035](decisions/0035-imu-messages.md)), orientation (w x y z), angular
velocity and acceleration, ten `f64` (80 bytes), NaN for what isn't
measured; and a `Walk` command for a walking robot, forward (m/s), left
(m/s) and turn (rad/s), three `f64` (24 bytes).

```rust
for state in State::read(&data) { /* state.position, state.velocity */ }
node.send_with("command", n * Command::LEN, |p| Command::write(&commands, p))?;
```

A command asks for the torque `kp (position − θ) + kd (velocity − θ̇) +
effort`: the MIT mode of quasi-direct-drive motors
([0031](decisions/0031-mit-commands-and-damiao.md)). Whoever drives the joint
closes that loop: a Damiao motor many times a cycle, `keel-sim` every step,
`keel-can` once a cycle for drives that take only a torque. A plain torque is
`Command::effort(τ)`, `kp = kd = 0`: what a controller closing its own loop,
like `keel-pid`, sends.

Every message also carries, in keel's header, a **stamp**: the moment of the
world it describes ([0028](decisions/0028-stamps.md)). A state's stamp is
when its joints were read, or the simulation's time; a command inherits the
stamp of the state it answers.

## Nodes

| node | what it is | flags |
|---|---|---|
| `keel-sim` | pendulums, stepped each tick | `--joints 1 --hz 1000 --cycle same\|next\|lockstep --deadline-us 300 --spin-us 0` |
| `keel-can` | Damiao's motors on a CAN bus: a cyclic bus master over SocketCAN | `--interface can0 --motors 0x01:0x11,… --fd false --hz 1000 --cycle same\|next --deadline-us 300 --spin-us 0 --names a,b` |
| `keel-dynamixel` | Dynamixel servos (Protocol 2.0) on a serial bus, and an IMU on it: a cyclic bus master | `--port /dev/ttyS2 --baud 1000000 --motors 20,21,… --p-gain 400 --imu 200 --imu-mount w,x,y,z --hz 50 --cycle same\|next --names a,b` |
| `keel-policy` | an ONNX policy: each state and IMU reading through a network, answered with positions | `--model velstand.onnx --joints a,b,… --walk 0,0,0 --head 0,0,0,0 --body 0,0,0,0,0,0 --lowpass 0.7 --lowpass-head 0.5 --action-scale 1` |
| `keel-keyboard` | a `Walk` command from the keys of a terminal running it: w/s ↑/↓, a/d ←/→, q/e, space | `--socket <path> --max 0.4,0.3,1.0` |
| `keel-pid` | a PID per joint, a command for each state | `--target 1.0 --kp 40 --ki 40 --kd 4 --limit 10 --spin-us 0 --period-us 0` |

## Damiao's motors

`keel-can --protocol damiao --motors 0x01:0x11,0x02:0x12 --fd true`: each
motor's CAN id and the id it answers on, in joint order, on CAN FD or
classic CAN.

- **At start**, each motor is put in MIT mode, its ranges are read from its
  registers, and it's enabled. A motor that doesn't answer is waited for,
  with a line in the log ("waiting for motor 0x01 on `vcan0`… is the robot,
  or drydock, running?"): the robot may be started after keel. **At stop**,
  they're disabled.
- **Each cycle, one round of frames**: a command to each motor, as soon as
  the controller answers, or the previous one again at the deadline. The
  motors answer each frame with their state, and those answers are the next
  cycle's `state`, stamped with when the round went out.
- **A motor's errors** (overheating, communication lost…) and changes of
  state go to the log, once each.

The frames are 8 bytes, classic or CAN FD; what CAN FD changes is their time
on the wire, at 1 Mbit/s with the data at 5 (OpenArm's settings):

| a cycle of 8 motors (8 commands, 8 answers) | classic | CAN FD |
|---|---:|---:|
| time on the bus | 1.8–2.2 ms | 0.8–0.9 ms |
| at 500 Hz | 90–110% | ~41% |
| at 1 kHz | doesn't fit | ~82% |

### OpenArm, in simulation

`examples/openarm.yml`: OpenArm v1's seven joints and gripper, with
`openarm-wave` holding the home pose, swinging the first joint and working
the gripper. The arm is drydock, on `vcan0`; in drydock's repository (it
fetches OpenArm's model the first time):

```sh
drydock examples/openarm/robot.yml    # the arm
```

then here, in either order (`keel-can` waits for the motors):

```sh
keel doctor examples/openarm.yml      # what's missing, with the command to fix it
keel run examples/openarm.yml
keel logs arm             # "motor 0x03: MIT mode, ±12.5 rad, ±10 rad/s, ±28 N m, answering on 0x13"
keel logs controller      # "3.0s joints at -0.40 -0.00 -0.00 +1.64 -0.00 -0.00 +0.06 -1.03 rad"
keel trace
```

On a real OpenArm, `--interface can0`; nothing else changes.

## Dynamixel servos

`keel-dynamixel --port /dev/ttyS2 --motors 20,21,22 --p-gain 200`: servos
on a half-duplex serial bus, by id, in joint order
([0034](decisions/0034-dynamixel-servos.md)).

- **A command's position is its servo's goal.** The servo closes its own
  loop, with the P gain set at start; a command's velocity, gains and
  effort aren't used, and the node says so once if they're sent.
- **At start**, every device is waited for, as on CAN, then each servo
  holds where it is: its goal its position, its P gain set, torque on. **At
  stop**, torque off. keel writes the servos' RAM only; how they're set up
  (ids, baud rate, return delay, homing offsets) is the robot's.
- **Each cycle, one Fast Sync Read** of every servo (PWM, current,
  velocity, position), published as `state`, stamped with when it was
  asked; then **one Sync Write** of the goals. A state's effort is the
  current times the XL330's torque constant (BAM's 0.366 N m/A), an
  estimate: the XL330 measures its supply's current.
- **An IMU on the bus** (`--imu 200`, the Microduck's) is read in the same
  Fast Sync Read, turned into the body's frame by `--imu-mount`, and
  published as `imu` ([0035](decisions/0035-imu-messages.md)), just before
  the state.

### The Microduck, in simulation

`examples/microduck.yml`: Pollen's Microduck, fifteen servos and its IMU,
with `microduck-stand` holding the pose its policies start from. The duck is
drydock, behind a pseudo-terminal; in drydock's repository, in lockstep, so
it stands until keel drives it:

```sh
drydock examples/microduck/robot.yml --lockstep 50
```

then here:

```sh
keel run examples/microduck.yml
keel logs controller      # "0.0s joints within 0.006 rad of standing; down [+0.00 -0.00 -1.00]"
```

A fixed pose doesn't balance it: servos that only hold positions sag under
its weight, it leans forward, and the lean feeds itself. At Pollen's P gain
(200) it tips over within two seconds; at the factory's (400) it leans
0.16 rad in seven. Its policy balances it.

## Policies

`keel-policy` runs a network trained in simulation, as an ONNX file, with
tract, in a node of its own: the one crate that depends on it
([0036](decisions/0036-policies-as-onnx.md)).

- **What it was trained with is in its file**: the joints it drives and
  their default pose, its action scale, and its observation's parts, by
  name. keel knows the parts of Pollen's Microduck policies: the gyro,
  gravity in the body's frame, joint positions from the default pose, their
  velocities, the last action, and the commands (`--walk`, `--head`,
  `--body`). A policy observing anything else is refused at start, by name.
- **Each state, with the latest IMU reading, gets a command**: the default
  pose plus the action scale times the network's output, low-passed as
  Pollen's runtime does (0.7 for the legs, 0.5 for the head: Pollen says
  they must match training). Its joints are found by name (`--joints`
  names the state's); the others hold where they were.

`examples/microduck-policy.yml`: the duck on Pollen's `velstand` policy, in
drydock as above. At Pollen's gain, where the fixed pose fell in two
seconds, it stands, gravity within 0.02 of straight down, and asked to walk
at 0.2 m/s, stays up (11 s, rocking ±0.06). Whether it goes forward isn't
visible from keel: drydock doesn't say where the duck is.

```sh
curl -fL https://huggingface.co/pollen-robotics/microduck-policies/resolve/main/velstand.onnx \
  -o examples/microduck/velstand.onnx
keel run examples/microduck-policy.yml
keel logs policy          # "1.0s down [-0.01 -0.00 -1.00]; walking +0.00 m/s forward, …"
```

**Steering it.** The dataflow's `keys` node is `keel-keyboard`: it publishes
a `Walk` command each cycle, which `keel-policy` takes in place of `--walk`.
A node can't read a terminal (keel starts nodes in their own process
group), so the same program, run in a terminal, sends that terminal's keys
to the node over a local socket:

```sh
target/debug/keel-keyboard     # in another terminal
```

```
w/s ↑/↓ forward and back, a/d ←/→ turn, q/e step aside, space stop, Ctrl-C quit
forward +0.10 m/s, left +0.00 m/s, turn +0.4 rad/s
```

Steps of 0.05 m/s and 0.2 rad/s, within the policy's training ranges. If
the terminal goes, the node asks the duck to stop.

## Watching joints

Every joints node watches its joints: the joints' loop compares each state
it reads with the command the joint was last given (`keel_control::health`).
When a joint, for half a second:

- **gives far less torque than asked**: the command's law, on the joint as
  it reads, asks for a torque; the drive reports giving under 30% of it;
- **stays far from its target**: more than 0.2 rad, with a position gain;
- **reads the same while its target moves**: a frozen sensor.

the joint's `state` says so, in its status flags, for the controller and
whatever else reads it, and the node's log says it in words, with the
numbers, and again when it's over; with a summary every 5 s. The drive's own
errors (Damiao's: overheating, overcurrent, communication lost…) go in the
same status, with the drive's code, and the log. Nothing counts in the first
2 s after the first command, while joints settle. Time is the states'
stamps. The thresholds are flags of the joints node (`--names`,
`--error-rad 0.2`, `--ratio 0.3`, `--min-asked 1`, `--hold-s 0.5`,
`--grace-s 2`, `--summary-s 5`).

```
4.8s joint 4 (openarm_joint4): gives 0.0 N m of the -52.4 asked; +0.87 rad from its target (1.57)
4.9s joint 1 (openarm_joint1): its position reads 0.160 rad, unchanged for 0.6 s, while its target went to 0.39
7.0s not following: joint 4 (openarm_joint4)
```

In `examples/openarm.yml`, `keel logs arm` shows them. drydock breaks motors on purpose,
as a scenario says (`examples/openarm/elbow-dies.scenario.yml` in drydock:
`dead`, `weak`, `freeze`, `noisy`, at set times or at random), and
`examples/agent/run.sh openarm-dead | openarm-weak | openarm-freeze |
openarm-healthy` asks an agent, with `keel-mcp`'s tools only, whether
anything is wrong with the arm, and grades the answer: which joint, what
kind of fault, or nothing on a healthy arm
([0033](decisions/0033-joints-say-what-is-wrong.md)). There's no central
watcher: each component says what's wrong with what it knows, and the agent
reads every component's log.

## Writing a driver

A node behind which there are joints is a driver and the joints' loop. The
driver implements `Joints`: what its joints need and nothing else.

```rust
impl Joints for MyArm {
    fn count(&self) -> usize { 6 }
    fn check(&mut self) -> io::Result<()> { /* for `keel doctor`: do the drives answer? */ Ok(()) }
    fn start(&mut self) -> io::Result<()> { /* modes, enable */ Ok(()) }
    /// Every joint's state and the moment it describes, once there's a whole one.
    fn read(&mut self, states: &mut [State]) -> io::Result<Option<u64>> { … }
    /// This cycle's commands, the newest there are (`None` before the first).
    fn write(&mut self, commands: Option<&[Command]>, states: &[State]) -> io::Result<()> { … }
    fn stop(&mut self) -> io::Result<()> { /* disable */ Ok(()) }
}

fn main() -> ExitCode {
    joints::exit_on(MyArm::open().and_then(|arm| joints::run(arm, Settings::from_args()?)))
}
```

`check` is for `keel doctor`: what's worth knowing before a run (`keel-can`
asks whether its motors answer). `joints::run` does a joints node's whole
`main` after opening its joints: the check if the doctor asked, joining the
dataflow, the loop.

`JointLoop` is the rest, the same for every driver: the tick, publishing
the state, waiting for the command within the cycle, writing it every cycle
(the previous one again when the controller is late), the log lines for late
commands and silent joints, and `stop` however the loop ends. `keel-can`
runs `DamiaoJoints`; `keel-sim` runs `Pendulums` ([0032](decisions/0032-a-loop-for-any-joints.md)).

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
thumbv7em-none-eabihf`) and has only run on a pretend chip so far: the same
loop over a pseudo-terminal (`examples/control-micro.yml`, the PID again).

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
  - id: camera
    path: ../target/debug/camera
    phase_us: 900          # 100 µs before the 1 ms loops' tick
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
        commands.extend(State::read(&data).map(|s| Command { effort: law(s, dt) }));
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

OpenArm against drydock, CAN FD, no real-time settings: at 500 Hz, state to
command 15–40 µs end to end, 3 commands in 500 past the 300 µs deadline, the
bus 41% busy; at 1 kHz, 83% busy (82% predicted), and about 5% of cycles
without a whole state, the answers crossing just before the next tick.

In lockstep, the simulation ran 1301 simulated seconds in 6 s, on the same
trajectory as in real time.

## Examples

```sh
keel run examples/control-sim.yml         # keel-pid holding a simulated pendulum at 1 rad, at 1 kHz
keel logs controller                      # "5s target 1: at 1.000 rad, +0.000 rad/s, pushing +4.13 N m"
keel trace                                # state → command, end to end
keel run examples/control-lockstep.yml    # the same in lockstep: simulated time goes by over 100× faster
keel run examples/control-micro.yml       # the joint on a (pretend) microcontroller
keel run examples/openarm.yml             # OpenArm on Damiao motors, against drydock (above)
```

## Not yet

- **A fresher state from drives that answer**: the state published at a
  tick is what the previous round's answers said. A second round within the
  cycle would shorten it, at twice the bus load.
- **Joint units for what isn't a joint's motor**: OpenArm's gripper is a
  motor's angle, not the fingers' opening. A transmission in `keel-can`, or
  the controller's business.
- **Several buses, one controller**: two arms, two masters in phase, and a
  controller that waits for both states of a cycle.
- **Cameras**, rendered by a simulation into shared memory.
- **Slow loops**: a policy every Nth cycle, and an interpolator playing its
  actions back by their stamps.
- **The age of a command** in `keel trace`: from its stamp to its sending.
