# keel

A minimal robotics-style middleware in Rust, built to learn how the whole
stack fits together: transport, runtime, lifecycle, coordination, packaging,
deployment, provisioning and tooling. See [docs/architecture.md](docs/architecture.md)
and the [decision records](docs/decisions/).

Today: a daemon spawns the nodes of a dataflow and routes messages between
them over a Unix socket.

```
crates/keel          node API
crates/keel-daemon   reads a dataflow YAML, spawns nodes, routes messages
examples/talker      \ a talker and a listener acknowledging each other
examples/listener    /
examples/bench       latency and throughput benchmark
```

## Run the example

```sh
cargo build
./target/debug/keel-daemon examples/dataflow.yml
```

Benchmark: `cargo build --release && ./target/release/keel-daemon examples/bench.yml`.

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
exited. If a node fails, the daemon kills the rest.
