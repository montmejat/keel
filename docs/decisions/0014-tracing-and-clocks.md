# 0014: Tracing: timestamps at every hop, clocks aligned by keel

**Status:** Accepted, 2026-09-30

## Context
Benchmarks ([0009](0009-measure-before-optimizing.md)) give one number for a
whole round trip. To make real-time work ([roadmap](../architecture.md#milestones)),
we need to know where the time goes: in the sender, the daemon, the network,
the receiver's processing. We also need to follow one message through
everything it causes, including across machines, whose clocks don't agree:
the laptop and the Pi each sync to internet time, which leaves them
milliseconds apart, when a local hop takes ~20 µs.

## Decision
- **Every message has an identity:** its source `node/output` and a sequence
  number. Its **trace** is the identity of the source message that started
  the chain.
- **Causality is implicit by default:** an output sent while handling an
  input carries that input's trace, and names it as its parent. A node that
  combines several inputs can name the cause explicitly.
- **The trace context travels with the payload:** trace, parent and publish
  time go in the region header (it has 64 bytes; the reference count uses
  4), and in `PeerMsg::Data` across machines. Receivers don't need to look
  anything up.
- **Hops record events, not header fields:** sender published, daemon
  forwarded, sent to and received from the network, delivered, taken by the
  receiver, released. Several receivers share one region, so their events
  can't live in it.
- **Events go to a ring buffer in shared memory, one per process:** no
  syscall and no lock on the hot path, just `clock_gettime` (~20 ns, vDSO)
  and a few stores. When the ring is full, events are dropped and counted,
  never waited on. The daemon drains the rings.
- **One timeline, the coordinator's:** timestamps are `CLOCK_MONOTONIC`
  on each machine. The coordinator measures each daemon's offset against
  its own clock with NTP-style exchanges (four timestamps) on the connection
  it already has, keeping the samples with the lowest round trip. Every
  cross-machine duration is shown with its uncertainty (`± 0.4 ms`) rather
  than as an exact number. Better clock sync (chrony on the LAN, PTP) only
  makes the error bar smaller; keel doesn't depend on it.
- **Tools:** the daemon keeps a latency histogram per hop and per link, and
  recent traces, and serves them on the control API. `keel trace` shows
  them. `keel trace --export` writes Chrome/Perfetto trace JSON, to look at
  a run on a timeline in ui.perfetto.dev.
- **A jitter benchmark** joins the throughput one: a periodic loop at 1 kHz
  measuring how late each wake-up is and the round trip through a node, as
  histograms including the maximum, idle and under load.

## Consequences
- Tracing is always on. Its cost gets measured with the existing benchmark
  before and after, and recorded like any other.
- The node API grows slightly (explicit causes), and the wire formats grow
  by a few fixed-size fields per message.
- At high rates (the benchmark does 300k msg/s) not every trace can be kept:
  histograms count everything, full traces are sampled.
- With the coordinator gone, traces from different machines can no longer
  be put on one timeline. Each machine's own traces stay valid.
- Across machines, precision is limited by how well the offset is known:
  a few hundred µs over Wi-Fi, better on Ethernet.
