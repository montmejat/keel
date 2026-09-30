# keel

A minimal robotics-style middleware in Rust, built to learn how the whole
stack fits together: transport, runtime, lifecycle, coordination, packaging,
deployment, provisioning and tooling. See [docs/architecture.md](docs/architecture.md)
and the [decision records](docs/decisions/).

Today: dataflows run on one machine or across several. On a machine, nodes
exchange payloads through shared memory, zero-copy, and a daemon forwards
small descriptors, supervises the nodes and serves a control API. Between
machines, daemons forward payloads over TCP. Linux only.

```
crates/keel          node API
crates/keel-daemon   runs dataflows: sessions, coordinator, daemon, control API
crates/keel-cli      the `keel` command, including the `keel top` TUI
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
it from anywhere:

```sh
./target/debug/keel daemon --listen 127.0.0.1:7401 &    # "robot"
./target/debug/keel daemon --listen 127.0.0.1:7402 &    # "base"
./target/debug/keel run examples/pipeline-two-machines.yml
```

Or with each machine in its own container (Podman):
`examples/containers/run.sh`. Daemons have no authentication: anyone who
can reach one can run programs through it, so only listen on trusted networks.

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

The daemon starts nodes only once every node has registered, so no message is
lost at startup. A node receives `Stop` once all of its upstream nodes have
exited. If a node fails, the daemon kills the rest. Stopping drains the
dataflow from its sources down; nodes that ignore `Stop` get SIGTERM, then
SIGKILL.
