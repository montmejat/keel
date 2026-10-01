# keel

A minimal robotics-style middleware in Rust, built to learn how the whole
stack fits together: transport, runtime, lifecycle, coordination, packaging,
deployment, provisioning and tooling. See [docs/architecture.md](docs/architecture.md)
and the [decision records](docs/decisions/).

Today: dataflows run on one machine or across several. On a machine, nodes
exchange payloads through shared memory, zero-copy, and pass descriptors to
each other through lock-free rings with futex wake-ups: 5.6 µs round trip,
5 M msg/s. A daemon per machine sets this up, supervises the nodes (with
real-time scheduling if asked), serves a control API, and forwards payloads
over TCP between machines. Every message is traced. Linux only.

```
crates/keel          node API
crates/keel-daemon   runs dataflows: sessions, coordinator, daemon, control API
crates/keel-cli      the `keel` command, including the `keel top` TUI
crates/keel-record   recordings: the file format and the recorder node
crates/keel-control  control, on top of the node API: joint messages, a PID,
                     a simulated joint, joints on a CAN bus
examples/            talker/listener, a camera pipeline, benchmarks, each also
                     across two machines; containers/ runs them in Podman
```

## Size

Lines of Rust that aren't blank or comments, at milestone 14:

| | lines |
|---|---:|
| `crates/keel` | 1381 |
| `crates/keel-daemon` | 3683 |
| `crates/keel-cli` | 1290 |
| `crates/keel-record` | 317 |
| `crates/keel-control` | 392 |
| `examples/` | 265 |
| **total** | **7328** |

Direct dependencies: `libc`, `serde`, `serde_json`, `serde_yaml`, and in the
CLI `clap` and `ratatui`. To count again:

```sh
find crates examples -name '*.rs' | xargs grep -cvE '^\s*(//|$)' | awk -F: '{n += $2} END {print n}'
```

## Try it

```sh
cargo build
./target/debug/keel run examples/pipeline.yml    # runs until stopped
```

In another terminal:

```sh
./target/debug/keel top          # live view: ↑↓ select, f filter logs, g graph, s stop, q quit
./target/debug/keel ps           # running dataflows
./target/debug/keel logs -f      # follow all logs; `keel logs camera` for one node
./target/debug/keel stop         # graceful stop (so is Ctrl-C in the first terminal)
```

### Across machines

Make a machine a keel machine (it needs SSH access, systemd, and ideally
passwordless sudo, for real-time limits):

```sh
keel provision pi            # as in your ~/.ssh/config; run from this repo
keel provision --remove pi   # undo it
```

It builds keel for the machine, installs it, runs the daemon as a systemd
service, and shares a token that every connection to a daemon must present.
The token authenticates but doesn't encrypt: use a VPN (WireGuard) on an
untrusted network.

List machines in the dataflow and place each node on one
(`examples/pipeline-two-machines.yml`), start a daemon per machine, then run
it from the machine with the sources. keel builds each node for its
machine's architecture and ships it (see below):

```sh
./target/debug/keel daemon --listen 127.0.0.1:7401 &    # "robot"
./target/debug/keel daemon --listen 127.0.0.1:7402 &    # "base"
./target/debug/keel run examples/pipeline-two-machines.yml
```

`keel top` then shows the whole dataflow, through the coordinator (`g` for
the graph, links across machines in yellow):

```
 keel   ● running   cluster base, robot   deployment ecc2c7f0451c   up 8s
╭ Nodes ──────────────────────────────────────────────────────────────────────
│  NODE       PROGRAM  MACHINE STATE       IN/s    OUT/s           OUT
│ ▌camera     camera   robot   ● running      0     30.8   182.9 MiB/s
│  detector   detector base    ● running   30.8     29.8       477 B/s
│  recorder   recorder robot   ● running   29.8        0         0 B/s
╭ Graph ──────────────────────────────────────────────────────────────────────
│              30.8/s 9.96ms                 29.8/s 278.5µs
│ [camera] ⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤>[detector] ⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤⠤>[recorder]
│  @robot                        @base                         @robot
```

### Deployments

A node with `build: <cargo binary>` instead of `path:` is built by keel, as
a reproducible static binary for its machine (x86_64 or aarch64), and sent
to that machine's daemon by SHA-256, only if it doesn't have it already.
Each deployment is recorded:

```sh
keel deploy examples/pipeline-two-machines.yml   # build and ship, don't run
keel history                                     # * marks the current one
keel rollback pipeline-two-machines              # back to the previous one
keel start pipeline-two-machines                 # run the current one
keel gc --keep 3                                 # forget older ones, delete unused binaries
```

A deployed dataflow has branches, to try something without losing what
works. Deployments go to the branch you're on, and `keel start` runs its
newest one:

```sh
keel branch pipeline-two-machines planner --new  # start a branch from the current deployment
keel deploy examples/pipeline-two-machines.yml   # lands on `planner`
keel diff pipeline-two-machines@main pipeline-two-machines   # node by node: binary, settings
keel branch pipeline-two-machines main           # back to main (no branch name: list them)
keel start pipeline-two-machines@planner         # run a branch without switching to it
keel merge pipeline-two-machines planner         # main takes what planner has
```

Cross-building needs the target: `rustup target add aarch64-unknown-linux-musl`
(and `x86_64-unknown-linux-musl`).

### Recording and replay

`keel-recorder` is a node that records its inputs to a file naming the
dataflow and deployment that produced them. `keel replay` runs a dataflow
with its recorded sources replaced by the recording, so the rest can't tell;
`keel export` turns a recording into files plus an index:

```sh
keel run examples/pipeline-recorded.yml                 # Ctrl-C to stop
keel recording ~/.local/share/keel/recordings/<file>    # channels, sizes
keel replay <file> examples/pipeline.yml --speed 2      # the camera, replayed
keel export <file> dataset/ --channel brightness --from 10 --to 20
```

With `--last <seconds>` the recorder is a flight recorder: it keeps only
the last seconds, in memory, and when a node fails the daemon saves them as
a recording that says which node and how. Replaying it runs the failure
again (`examples/flight-recorder.yml`, where a node crashes on purpose):

```sh
keel run examples/flight-recorder.yml
#   [daemon] `blackbox` held the 2.5s before `flaky` failed, 252 messages: saved to ...
keel replay <file> examples/flight-recorder.yml         # `flaky` crashes again, at the same count
```

Or with each machine in its own container (Podman):
`examples/containers/run.sh`. A daemon started by hand without a token (as
in these examples) accepts any connection: only on trusted networks.

### Where the time goes

Every message is traced. `keel trace` shows each input's latency (publish →
taken) and processing time (taken → released) as percentiles, then the latest
sampled traces: one message followed through every hop and every node it
caused, across machines too.

```sh
./target/debug/keel trace                 # while a dataflow runs
./target/debug/keel trace --export t.json # open in ui.perfetto.dev
```

```
camera/frames@robot #11: 7.63s end to end
  → detector/frames@base  7.49s  [send 6.51s, daemon 680ms, network 302ms, route 1.30ms, wake 42.5µs]  then processing 140µs
    detector/brightness #11
      → recorder/brightness@robot  132ms  [send 18.2µs, daemon 34.8µs, network 130ms, route 86.9µs, wake 2.01ms]  then processing 11.8µs
```

(A Raspberry Pi on Wi-Fi sending 6 MB frames to a laptop: the frame waited
6.5 s in the camera's queue before its daemon could send it.)

### Control

`keel-control` is a layer above keel, not part of it: nodes that publish a
joint `state` and take a `command`, and a controller between them. The
controller doesn't know what the joints are, so the same one runs against
a simulation and against a CAN bus:

```sh
keel run examples/control-sim.yml    # keel-pid holding a simulated pendulum at 1 rad, at 1 kHz
keel logs controller                 # "5s target 1: at 1.000 rad, +0.000 rad/s, pushing +4.13 N m"
```

`examples/control-can.yml` puts the joint behind a CAN bus (SocketCAN, a
raw socket): `keel-can` is the bus master, and `keel-can-motor` stands in
for the drive at the other end of a virtual interface. No hardware, and no
root either, in a network namespace:

```sh
unshare -rn sh -c 'ip link add vcan0 type vcan && ip link set vcan0 up &&
  ./target/debug/keel run examples/control-can.yml'
```

Other examples: `keel run examples/dataflow.yml` (talker/listener), and the
benchmarks: `cargo build --release`, then `./target/release/keel run examples/bench.yml`
(latency, throughput) or `examples/jitter.yml` (a 1 kHz loop).

## Dataflow

```yaml
nodes:
  - id: listener
    path: ../target/debug/listener   # relative to this file
    inputs:
      count: talker/count            # <input>: <node>/<output>
```

An input can also keep only the newest message, so that a slow reader (a
recorder, a viewer) never holds its sender back; and a node can ask for
real-time scheduling:

```yaml
  - id: controller
    path: ../target/release/controller
    rt: { priority: 80, cpus: [3] }  # SCHED_FIFO 80, pinned to CPU 3
    inputs:
      state: estimator/state
      camera: { source: camera/frames, keep: latest }
```

Nodes can also be restarted when they fail, watched for being stuck, and
replaced in a running dataflow (`examples/lifecycle.yml` shows the first
two, on purpose):

```yaml
  - id: detector
    build: detector
    restart: on-failure     # or always; max_restarts: 5, backoff 100 ms → 5 s
    watchdog_ms: 500        # killed (then restarted) after 500 ms without progress
```

```sh
keel update examples/pipeline-two-machines.yml   # new code in, node by node
```

Real-time priority needs the privilege: an rtprio limit (e.g.
`/etc/security/limits.d/keel.conf` with `<user> - rtprio 95`), or
`CAP_SYS_NICE`. Without it the daemon says so, and the rest applies.

The daemon starts nodes only once every node has registered, so no message is
lost at startup. A node receives `Stop` once all of its upstream nodes have
exited. If a node fails, the daemon kills the rest. Stopping drains the
dataflow from its sources down; nodes that ignore `Stop` get SIGTERM, then
SIGKILL.
