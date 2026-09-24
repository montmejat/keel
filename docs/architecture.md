# Architecture

keel is a learning project: a minimal but complete robotics-style middleware
in Rust, from packaging a node to running it across machines. Each layer is
the smallest thing that works end-to-end; the interest is in the layers and
the boundaries between them, not in supporting real robots.

Decisions and their reasoning live in [`decisions/`](decisions/).

## The stack

| Layer | Question it answers | Minimal version |
|---|---|---|
| Transport | How do bytes move? | Shared memory on one machine (zero-copy), TCP between machines |
| Runtime | Who runs the nodes on a machine? | A per-machine daemon |
| Lifecycle | Start, stop, crash, restart | Ordered startup and shutdown, restart policies |
| Coordination | Which node runs where? | A coordinator that assigns nodes to daemons |
| Packaging | What exactly gets shipped? | Static binary + config in a content-addressed bundle |
| Deployment | How does a bundle reach a machine? | `keel deploy` pushes bundles; daemons cache them by hash |
| Provisioning | How does a bare machine become a keel machine? | `keel provision <host>` over SSH installs and starts the daemon |
| Tooling | What's going on right now? | A `keel` CLI and TUI, clients of the daemons' control API |

## Planes

- **Data plane**: node-to-node messages. Must be fast; the daemon should not
  touch payloads once shared memory lands.
- **Control plane**: registration, lifecycle, deployment, and the state and
  metrics the tools display. Can be slow and simple.

Keeping these separate is the main structural rule: the TUI, the coordinator
and deployment all talk to the control plane only.

## Milestones

1. **Cleanup** (done): Rust only; daemon split into library + binary;
   benchmark baseline.
2. **Zero-copy local transport**: nodes exchange shared-memory buffers, the
   daemon only handles control messages. Measured against the baseline.
3. **Control API + first TUI**: daemon exposes its state; `keel top` shows
   nodes, lifecycle state, message rates.
4. **Multi-machine**: coordinator, several daemons, TCP between machines,
   tested with containers.
5. **Packaging and deployment**: bundles, hashing, push and cache.
6. **Provisioning**: SSH bootstrap of a fresh machine.
7. **Lifecycle polish**: restart policies, health checks, rolling updates.

## Baseline (milestone 1)

`cargo build --release && ./target/release/keel-daemon examples/bench.yml`.
Every message is copied into the daemon and out again over one Unix socket.

```
    size    rtt p50    rtt p99   throughput      msg/s
     64B     20.1µs     59.5µs        22 MB/s     342654
    4KiB     25.5µs     74.2µs       845 MB/s     206200
   64KiB     85.8µs    182.0µs      1221 MB/s      18625
    1MiB      3.3ms      4.2ms       548 MB/s        523
    8MiB     28.6ms     31.4ms       435 MB/s         52
```

Large messages are where zero-copy should matter: an 8 MiB message (roughly a
raw 4K camera frame) takes ~29 ms round trip today, too slow for 30 Hz.
