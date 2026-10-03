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
 drives              real drives               |  keel-can-motor (a stand-in)
```

**The dataflow is the hardware interface.** Whatever publishes `state` and
takes `command` is the joints; there's no driver trait and no plugin. The
controller can't tell a simulation from a bus, or a live run from a replay
or from a simulation in lockstep, and nothing above the joints should try
to: swapping one for the other is a change to the dataflow file.

A simulation can also sit lower, below the wire: `keel-can-motor` plays the
drives at the other end of a virtual CAN bus, so the bus master and its
protocol run in simulation as they would on a robot.

## Messages

Payloads are raw bytes to keel ([0003](decisions/0003-raw-byte-payloads.md));
this crate gives two of them a layout. One message carries every joint of a
bus or simulation, back to back, little-endian:

| message | per joint | bytes |
|---|---|---:|
| `State` | position (rad), velocity (rad/s), as `f64` | 16 |
| `Command` | effort (N m), as `f64` | 8 |

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
| `keel-sim` | pendulums, stepped each tick | `--joints 1 --hz 1000 --cycle same\|next\|lockstep --deadline-us 300 --spin-us 0` |
| `keel-can` | a cyclic bus master over SocketCAN | `--interface can0 --joints 1 --hz 1000 --cycle same\|next --deadline-us 300 --spin-us 0` |
| `keel-can-motor` | the drives at the other end of a (virtual) bus | `--interface vcan0 --joints 1 --hz 1000` |
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
  - id: motor
    path: ../target/debug/keel-can-motor
    phase_us: 900          # report 100 µs before the bus master's tick
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

On a virtual CAN bus, a command reaches `keel-can` 4–5 µs after it's sent,
where it used to wait 950 µs for the next tick; the stand-in drive applies
it when its frame arrives. In lockstep, the simulation ran 1301 simulated
seconds in 6 s, on the same trajectory as in real time.

## Examples

```sh
keel run examples/control-sim.yml         # keel-pid holding a simulated pendulum at 1 rad, at 1 kHz
keel logs controller                      # "5s target 1: at 1.000 rad, +0.000 rad/s, pushing +4.13 N m"
keel trace                                # state → command, end to end
keel run examples/control-lockstep.yml    # the same in lockstep: simulated time goes by over 100× faster
keel run examples/control-micro.yml       # the joint on a (pretend) microcontroller
```

Behind a virtual CAN bus: in a network namespace, without root, where the
system allows unprivileged ones, or with `sudo ip link add vcan0 type vcan &&
sudo ip link set vcan0 up` once:

```sh
unshare -rn sh -c 'ip link add vcan0 type vcan && ip link set vcan0 up &&
  ./target/debug/keel run examples/control-can.yml'
```

## Not yet

- **Commands the drives take**: position, velocity, stiffness, damping and
  torque (the MIT mode of quasi-direct-drive motors), and their protocol
  over CAN-FD, in place of the made-up frames.
- **A fresh state from request/response drives**: drives that answer each
  command with their state report one taken a cycle ago. A state request
  just before the tick (a phase) fixes it.
- **Several buses, one controller**: two arms, two masters in phase, and a
  controller that waits for both states of a cycle.
- **A simulation worth the name**: MuJoCo behind the virtual bus, and
  cameras rendered into shared memory.
- **Slow loops**: a policy every Nth cycle, and an interpolator playing its
  actions back by their stamps.
- **The age of a command** in `keel trace`: from its stamp to its sending.
