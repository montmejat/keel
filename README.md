# keel

A minimal robotics-style middleware in Rust, built to learn how the whole
stack fits together: transport, runtime, lifecycle, coordination, packaging,
deployment, provisioning and tooling. See [docs/architecture.md](docs/architecture.md)
and the [decision records](docs/decisions/).

Today: a daemon runs the nodes of a dataflow on one machine. Nodes exchange
payloads through shared memory, zero-copy; the daemon forwards small
descriptors, supervises the nodes and serves a control API. Linux only.

```
crates/keel          node API
crates/keel-daemon   runs a dataflow: spawns, routes, supervises, control API
crates/keel-cli      the `keel` command, including the `keel top` TUI
examples/            talker/listener, a camera pipeline, a benchmark
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

Other examples: `keel run examples/dataflow.yml` (talker/listener), and the
benchmark: `cargo build --release && ./target/release/keel run examples/bench.yml`.

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
