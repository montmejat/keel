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

## Linux building blocks

keel leans on the kernel and on Linux conventions instead of reinventing them:

| Need | Linux mechanism |
|---|---|
| Isolation between nodes | Processes |
| Zero-copy payloads | Files in `/dev/shm` (tmpfs) + `mmap(MAP_SHARED)` |
| Buffer ownership | Atomic reference counts in the shared pages |
| Control and discovery | Unix sockets in `$XDG_RUNTIME_DIR`, found by listing a directory |
| Liveness of a daemon | `/proc/<pid>` |
| Ctrl-C goes to the supervisor only | Process groups |
| Nodes die with the daemon | `prctl(PR_SET_PDEATHSIG)` |
| Stop, then insist | `Stop` message → SIGTERM → SIGKILL |
| Logs | stdout/stderr pipes, prefixed lines |

The closest relative on Linux is PipeWire (memfd/shared-memory buffers
between processes, a daemon brokering the graph). The supervision side
overlaps with systemd.

## Milestones

1. **Cleanup** (done): Rust only; daemon split into library + binary;
   benchmark baseline.
2. **Zero-copy local transport** (done): nodes exchange shared-memory
   buffers, the daemon only forwards descriptors.
3. **Control API + first TUI** (done): the daemon serves its state over a
   control socket; `keel ps | logs | stop | top` are clients of it. The
   daemon also became a proper supervisor: log capture, ordered stop.
4. **Multi-machine**: coordinator, several daemons, TCP between machines,
   tested with containers.
5. **Packaging and deployment**: bundles, hashing, push and cache.
6. **Provisioning**: SSH bootstrap of a fresh machine.
7. **Lifecycle polish**: restart policies, health checks, rolling updates.

## Benchmarks

`cargo build --release && ./target/release/keel run examples/bench.yml`.
Round-trip latency with one message in flight, then one-way throughput.

**Milestone 1**: every payload copied into the daemon and back out over a
Unix socket.

```
    size    rtt p50    rtt p99   throughput      msg/s
     64B     20.1µs     59.5µs        22 MB/s     342654
    4KiB     25.5µs     74.2µs       845 MB/s     206200
   64KiB     85.8µs    182.0µs      1221 MB/s      18625
    1MiB      3.3ms      4.2ms       548 MB/s        523
    8MiB     28.6ms     31.4ms       435 MB/s         52
```

**Milestone 2**: shared memory, only descriptors go through the daemon
([0005](decisions/0005-shared-memory-transport.md)).

```
    size    rtt p50    rtt p99   throughput      msg/s
     64B     24.9µs     58.1µs        18 MB/s     284499
    4KiB     23.1µs     53.1µs      1378 MB/s     336495
   64KiB     16.5µs     63.3µs     22987 MB/s     350749
    1MiB     21.6µs     70.1µs    184972 MB/s     176403
    8MiB     21.7µs     67.6µs    953293 MB/s     113641
```

**Milestone 3**: the daemon now counts messages per output and captures
node output through pipes. Latency is unchanged; small-message throughput is
10–20% lower (e.g. 64 B: 257k msg/s, 8 MiB: 100k msg/s).

With shared memory, latency is flat at ~20 µs whatever the size: an 8 MiB frame went from
29 ms to 22 µs round trip. The MB/s column is size × msg/s. Nothing reads or
writes the payload, so it isn't memory bandwidth; `msg/s` is the meaningful
number. A real producer writing an 8 MiB frame pays for that write once, in
place, and nothing else does.
