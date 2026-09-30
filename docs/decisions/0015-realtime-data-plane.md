# 0015: Real-time data plane: nodes talk directly, the daemon only sets up

**Status:** Proposed, 2026-09-30

## Context
The jitter benchmark ([architecture](../architecture.md#jitter)) shows a
1 kHz loop missing 11–13% of its deadlines under CPU load, with round trips
up to 15 ms. Every local message crosses two Unix sockets and a daemon
thread that has to be scheduled; every receive allocates; nodes run with
normal scheduling and 50 µs of timer slack. Tracing
([0014](0014-tracing-and-clocks.md)) can now measure each of these.

## Decision
- **Descriptors go node to node through shared memory.** For each input of
  each node, the daemon creates a *channel* file: a single-producer,
  single-consumer ring of descriptors (`slot`, `len` packed in one atomic
  word). The sender pushes to every target's channel, with one reference
  per target taken on the region beforehand. The daemon is not involved.
- **Wake-ups are futexes.** Each node has a *bell* file: a sequence number
  that senders bump after pushing, and a flag saying the receiver is asleep,
  so a busy receiver costs senders no syscall. Receivers wait with
  `FUTEX_WAIT` on the shared word (not `FUTEX_PRIVATE`, since it's shared
  between processes).
- **The daemon only sets up, forwards, and supervises.** At registration it
  tells each node its routes. It still receives from and sends to other
  machines: an output with targets elsewhere also has a channel read by the
  daemon, and messages from elsewhere are pushed by the daemon into local
  channels exactly as a local sender would. Stop is a flag in the bell. The
  node's socket stays open only to detect its exit.
- **Two policies per input, set in the dataflow:** `keep: all` (the default):
  every message is queued, and a sender whose 32 regions are all held waits
  (backpressure, as before). `keep: latest`: the channel holds one message
  and a newer one replaces it, releasing the older. A slow receiver (a
  recorder, a viewer) then never slows its sender.
- **No allocation per message** once a node is running: input names are
  static, regions are looked up by index, samples hold no heap data.
- **Real-time scheduling from the dataflow:**
  ```yaml
  - id: controller
    rt: { priority: 80, cpus: [3] }   # SCHED_FIFO, pinned to CPU 3
  ```
  The daemon applies it right after spawning the node (`sched_setscheduler`,
  `sched_setaffinity`). Without the privilege (`RLIMIT_RTPRIO` is 0 for
  normal users), it logs a warning and the node runs with normal
  scheduling, still pinned. RT nodes also get timer slack of 1 ns and lock
  their memory (`mlockall`) when the memory lock limit allows it.
- **Periodic loops:** `Periodic` wakes on absolute deadlines
  (`clock_nanosleep(TIMER_ABSTIME)`), reports lateness, and counts missed
  periods instead of bursting to catch up. `Node::try_next_event` reads
  inputs without blocking, for loops that must not wait.

## Consequences
- Local latency no longer includes a daemon; the daemon no longer counts
  local messages, so per-output counters move to the sender's stats file.
- Descriptor order across different inputs is no longer global: a node
  reads its inputs round-robin. Order within an input is kept.
- A node that dies mid-send can leave a reference it took for a target.
  Its regions are freed with the session, as before.
- One thread forwards all of a machine's outputs to other machines, so a
  slow link delays the others' forwarding.
- Privileges for real-time scheduling are the machine's business
  (`/etc/security/limits.d`, or `CAP_SYS_NICE`); keel says what it got.

## Measured (milestone 6)
- Local round trip 20 → 5.6 µs, small messages 270k → 5.4 M msg/s.
- 1 kHz loop under full CPU load: missed deadlines 11–13% → 1–2.5% from the
  data path alone; 0 on the Pi with SCHED_FIFO, worst round trip 331 µs,
  stock kernel. Tables in [architecture](../architecture.md#jitter).
- musl's `sched_setscheduler` always fails (`ENOSYS`: Linux sets it per
  thread, POSIX per process), so the daemon makes the raw syscall on the
  node's main thread.
