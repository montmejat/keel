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
| Real time | Can a control loop meet its deadlines? | SCHED_FIFO, pinned CPUs, no daemon on the data path |
| Runtime | Who runs the nodes on a machine? | A per-machine daemon |
| Lifecycle | Start, stop, crash, restart | Ordered startup and shutdown, restart policies, a watchdog, rolling updates |
| Coordination | Which node runs where? | A coordinator that assigns nodes to daemons |
| Packaging | What exactly gets shipped? | Reproducible static binaries, identified by their hash |
| Deployment | How does software reach a machine? | `keel deploy` pushes missing hashes to a store on each daemon; rollback and cleanup |
| Provisioning | How does a bare machine become a keel machine? | `keel provision <host>` over SSH installs and starts the daemon |
| Observability | Where does the time go? | Timestamps at every hop, traces from a source to everything it caused, clocks aligned by keel |
| Recording | What happened, and can it happen again? | A recorder node, replay in place of the sources, export to datasets |
| Tooling | What's going on right now? | A `keel` CLI and TUI, clients of the daemons' control API |

## Processes

```
                  ┌─────────────────────────┐
                  │ keel run  (coordinator) │
                  └───┬─────────────────┬───┘
                      │                 │     control: JSON lines over TCP
                      │                 │     (spawn, start, stop, abort;
                      │                 │     events and logs back)
            ┌─────────┴─────┐     ┌─────┴─────────┐
            │  keel daemon  │────>│  keel daemon  │   data: PeerMsg over TCP,
            │    "robot"    │<────│    "base"     │   one connection per direction
            └──┬─────────┬──┘     └───────┬───────┘
               │         │                │     Unix socket + /dev/shm
            camera   recorder          detector
```

For a single-machine dataflow, `keel run` is the coordinator and the only
daemon in one process.

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
| Descriptors between nodes | Lock-free rings in `/dev/shm` files |
| Waking a receiver | `futex` on a word in shared memory (not `FUTEX_PRIVATE`) |
| Real-time nodes | `SCHED_FIFO`, `sched_setaffinity`, `mlockall`, `PR_SET_TIMERSLACK` |
| Periodic loops | `clock_nanosleep(TIMER_ABSTIME)` |
| Timestamps | `clock_gettime(CLOCK_MONOTONIC)`, a vDSO call (~20 ns) |
| Binaries that run anywhere | Static musl builds, linked by `rust-lld` |
| Naming software | SHA-256 of the binary |
| Switching versions atomically | A symlink replaced with `rename(2)` |
| Bootstrapping a machine | `ssh`, piping the binary through its stdin |
| Running the daemon | A systemd unit (`LimitRTPRIO`, `LimitMEMLOCK`); logs in the journal |
| Authenticating connections | A 256-bit token from `/dev/urandom`, shared by `keel provision` |
| Trace data out of the nodes | Lock-free rings and counters in `/dev/shm` files |
| Aligning machines' clocks | NTP's four-timestamp exchange, over the coordinator's own connection |

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
4. **Multi-machine** (done): `keel daemon` per machine, `keel run` as the
   coordinator, TCP between daemons, tested with two local daemons and with
   Podman containers.
5. **Tracing and latency** (done): timestamps at every hop, traces that follow a
   message through every node it causes, clock offsets measured by keel
   itself, `keel trace`, a Perfetto export, and a jitter benchmark
   ([0014](decisions/0014-tracing-and-clocks.md)). This comes first because it is
   what the next milestones get measured with.
6. **Real-time data plane** (done): the daemon off the data path (descriptor rings
   in shared memory, futex wake-ups), no allocation per message, SCHED_FIFO
   and CPU pinning from the dataflow, per-input policies (block, or keep the
   latest), a periodic-loop helper. Tried on a PREEMPT_RT kernel on the Pi.
7. **Packaging and deployment** (done): reproducible static builds, a store keyed
   by hash on each daemon, `keel deploy | rollback | history | gc`. Replaces
   copying binaries to the same path on every machine.
8. **Recording and replay** (done): a recorder node, a file format that names the
   deployment that produced it, `keel replay`, `keel export` to datasets.
9. **Cluster TUI** (done): `keel top` through the coordinator: the whole graph
   across machines, latency per link, traces, deployed version, recordings.
10. **Provisioning** (done): `keel provision <host>` over SSH installs keel,
    runs the daemon as a systemd service with real-time limits, and shares a
    token every connection must present
    ([0018](decisions/0018-provisioning.md)).
11. **Lifecycle polish** (done): restart policies with backoff, a progress
    watchdog, rolling updates of a running dataflow
    ([0019](decisions/0019-lifecycle.md)).
12. **Branches** (done): a deployed dataflow has named branches; `keel
    branch | diff | merge` ([0020](decisions/0020-branches.md)). Not yet: a
    branch running beside the live dataflow, and cloning from another
    machine.
13. **Control, first step** (done): `keel-control`, a crate above the node
    API: joint state and command messages, a PID node, a simulated joint,
    and joints on a CAN bus, tried on a virtual one
    ([0021](decisions/0021-control-layer.md)). Not yet: EtherCAT, MuJoCo, a
    simulated clock.
14. **Flight recorder** (done): a recorder that keeps the last seconds in
    memory; the daemon saves them when a node fails, as a recording that
    replays the failure ([0022](decisions/0022-flight-recorder.md)).
15. **keel as process 1** (done): `keel image` builds an initramfs holding
    keel, a network driver and the token; booted by a kernel, keel is the
    machine's only program ([0023](decisions/0023-process-one.md)).
16. **Fleet** (done): a fleet file lists robots running one dataflow; `keel
    fleet run | status | update | rollback`, with `--only` to try new code
    on some robots first ([0024](decisions/0024-fleet.md)).
17. **A node on a microcontroller** (done, without hardware): `keel-micro`,
    a `no_std` node over a serial link, and `keel-serial`, the node standing
    in for it ([0025](decisions/0025-microcontroller-nodes.md)).

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

**Milestone 4, across machines**: source and sink on two daemons on one
host (`examples/bench-two-machines.yml`), so every message crosses TCP.

```
    size    rtt p50    rtt p99   throughput      msg/s
     64B     57.9µs    730.4µs        10 MB/s     151343
    4KiB     59.0µs    133.3µs       556 MB/s     135731
   64KiB     71.9µs    211.8µs      3564 MB/s      54386
    1MiB    476.1µs    629.2µs      2592 MB/s       2472
    8MiB      5.6ms      7.3ms      1437 MB/s        171
```

Unlike the shared-memory numbers, these include real copies of the payload.

**Milestone 6**: descriptors go node to node through shared-memory rings,
woken by futexes; the daemon is off the local path
([0015](decisions/0015-realtime-data-plane.md)).

```
    size    rtt p50    rtt p99   throughput      msg/s
     64B      5.6µs     10.8µs       344 MB/s    5368900
    4KiB      6.6µs     10.3µs     22264 MB/s    5435516
   64KiB      5.5µs      9.2µs    294128 MB/s    4488039
    1MiB      5.6µs      8.0µs   1285921 MB/s    1226350
    8MiB      5.5µs      7.4µs   1845437 MB/s     219993
```

Round trips are 3.5× faster and small-message throughput is 20× higher. On
the Pi 3: 25 µs round trip, 750k msg/s.

Across two daemons on one host, M6 also fixed a copy-per-field in the wire
format's parser that M5 had made worse (five trace fields in front of the
payload): 64 B 40 µs, 1 MiB 390 µs, 8 MiB 3.9 ms round trip (M4: 58 µs,
476 µs, 5.6 ms).

With shared memory, latency is flat at ~20 µs whatever the size: an 8 MiB frame went from
29 ms to 22 µs round trip. The MB/s column is size × msg/s. Nothing reads or
writes the payload, so it isn't memory bandwidth; `msg/s` is the meaningful
number. A real producer writing an 8 MiB frame pays for that write once, in
place, and nothing else does.

### Jitter

`examples/jitter.yml`: a 1 kHz loop wakes on an absolute deadline
(`clock_nanosleep`), then does a 64 B round trip through keel. 10,000 ticks.
"Loaded" is one `yes > /dev/null` per core. Milestone 5 baseline, before any
real-time work: normal scheduling, no pinning, stock kernels.

```
                        wake-up late                   round trip             missed
                    p50     p99   p99.9    max     p50     p99   p99.9    max
laptop  idle     96.8µs   146µs   191µs  384µs   328µs   437µs   529µs  1.3ms      0.0%
laptop  loaded   56.3µs   1.8ms   3.7ms  7.7ms  65.4µs   2.8ms   4.0ms 14.8ms     11.1%
Pi 3    idle     67.6µs  75.9µs   123µs  4.0ms   176µs   245µs   816µs  3.2ms      0.2%
Pi 3    loaded   62.6µs  67.8µs   3.7ms  3.7ms   127µs   4.7ms   5.7ms  8.7ms     12.7%
```

(Laptop: 12 cores, Fedora `PREEMPT_DYNAMIC`. Pi: 4 cores, 6.12 `rpi-v8`.)

Idle, the median round trip is 330 µs against ~20 µs back to back: between
ticks the CPUs sleep in low-power states and take time to wake, and the
default 50 µs timer slack lets wake-ups run late on purpose. Loaded, the
median improves since no CPU sleeps, but the tail reaches milliseconds and one
deadline in nine is missed. A control loop cares about the max, not the
median.

**After milestone 6** (same benchmark; the Pi gained an rtprio limit, the
laptop didn't, so its "rt" runs only get pinning, 1 ns timer slack and
locked memory):

```
                        wake-up late                   round trip             missed
                    p50     p99   p99.9    max     p50     p99   p99.9    max
laptop  idle     91.2µs   157µs   488µs  2.9ms   115µs   169µs   246µs  2.2ms      0.1%
laptop  loaded   23.3µs  49.4µs   1.9ms  4.6ms  17.9µs  22.9µs   2.7ms  4.1ms      1.3%
laptop  rt load   5.8µs  97.1µs   633µs  1.8ms  16.5µs   625µs   2.6ms  3.5ms      0.8%
Pi 3    idle     61.4µs  75.5µs   200µs  1.4ms  29.6µs  88.1µs   235µs  2.2ms      0.1%
Pi 3    loaded   62.3µs  69.5µs   3.1ms  5.1ms  35.5µs  80.5µs   5.0ms  6.0ms      2.5%
Pi 3    rt idle  12.9µs  20.7µs  41.7µs 60.6µs  39.5µs  59.2µs  74.1µs  325µs      0.0%
Pi 3    rt load   9.6µs  12.2µs  19.9µs 85.8µs  25.4µs  32.0µs  53.0µs  331µs      0.0%
```

Taking the daemon off the path cut missed deadlines under load from 11–13%
to 1–2.5%. SCHED_FIFO on the Pi (`examples/jitter-rt.yml`) removes them:
with every core busy, the worst wake-up is 86 µs late and the worst round
trip 331 µs, on the stock (not PREEMPT_RT) kernel.

### Tracing

Milestone 5 adds a clock read and a few atomic adds per message on each
side, plus a mutex and a hash lookup on receipt. Alternating runs of the
benchmark with and without it on the laptop overlap completely (64 B round
trip 18–27 µs without, 21–27 µs with; 243–262k vs 231–268k msg/s): the cost
is below this machine's run-to-run noise.
