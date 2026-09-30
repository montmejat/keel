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
examples/            talker/listener, a camera pipeline, benchmarks, each also
                     across two machines; containers/ runs them in Podman
```

## Try it

```sh
cargo build
./target/debug/keel run examples/pipeline.yml    # runs until stopped
```

In another terminal:

```sh
./target/debug/keel top          # live view: ↑↓ select, f filter logs, s stop, q quit
./target/debug/keel ps           # running dataflows
./target/debug/keel logs -f      # follow all logs; `keel logs camera` for one node
./target/debug/keel stop         # graceful stop (so is Ctrl-C in the first terminal)
```

### Across machines

List machines in the dataflow and place each node on one
(`examples/pipeline-two-machines.yml`), start a daemon per machine, then run
it from the machine with the sources. keel builds each node for its
machine's architecture and ships it (see below):

```sh
./target/debug/keel daemon --listen 127.0.0.1:7401 &    # "robot"
./target/debug/keel daemon --listen 127.0.0.1:7402 &    # "base"
./target/debug/keel run examples/pipeline-two-machines.yml
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

Or with each machine in its own container (Podman):
`examples/containers/run.sh`. Daemons have no authentication: anyone who
can reach one can run programs through it, so only listen on trusted networks.

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

Real-time priority needs the privilege: an rtprio limit (e.g.
`/etc/security/limits.d/keel.conf` with `<user> - rtprio 95`), or
`CAP_SYS_NICE`. Without it the daemon says so, and the rest applies.

The daemon starts nodes only once every node has registered, so no message is
lost at startup. A node receives `Stop` once all of its upstream nodes have
exited. If a node fails, the daemon kills the rest. Stopping drains the
dataflow from its sources down; nodes that ignore `Stop` get SIGTERM, then
SIGKILL.
