# 0005: Zero-copy local transport over shared memory

**Status:** Proposed, 2026-09-24. Implemented in milestone 2, awaiting review.

## Context
Baseline ([architecture](../architecture.md#benchmarks)): an 8 MiB message
took ~29 ms round trip and throughput peaked around 1.2 GB/s, because every
payload was copied into the daemon and back out over a Unix socket.

## Decision
Payloads live in shared memory; the socket only carries descriptors.

- **Regions.** Each node owns up to 32 regions, one file each in
  `/dev/shm/keel-<daemon pid>/<node>.<slot>`, mapped with `mmap`. A region is
  a 64-byte header holding an atomic reference count, then the payload.
- **Sending.** `send_with(output, len, |buf| ...)` picks a free region (count
  0), lets the node write the payload in place, sets the count to 1 (the
  reference held by the message in transit) and sends `Output { slot, len }`.
  `send_output(&[u8])` does the same with one copy.
- **Routing.** The daemon adds one reference per receiver before forwarding
  `Input { source, slot, len }`, gives back the reference of any delivery
  that fails, then drops the in-transit reference. It only ever touches the
  header.
- **Receiving.** Nodes map regions on first use and get a `Sample` that
  derefs to `&[u8]` in place. Dropping it releases the reference.
- **Reuse and growth.** A sender only writes to or grows a region whose count
  is 0, so no reader ever sees a payload change. Regions grow to the next
  power of two when a bigger payload needs one, and receivers remap when
  they see a length larger than their mapping.
- **Backpressure.** When all 32 regions are held, the sender waits: it spins,
  then sleeps. After 10 s it fails with an error naming the cause.
- **Cleanup.** The daemon removes its directory on exit, and on startup it
  removes directories left by daemons that no longer run.
- **By hand.** Implemented with `libc::mmap` rather than `iceoryx2` or
  `memmap2`, since learning how this works is the point.

Descriptors still go through the daemon. So latency is still two socket hops
(~20 µs), but it no longer depends on message size. Direct node-to-node
notifications (eventfd, futex) are a possible later step.

## Consequences
- Latency no longer depends on message size; see the benchmarks.
- Small-message throughput dropped about 15% (342k → 284k msg/s) from the
  atomics and the 32-slot cap. Acceptable for now.
- A receiver that crashes while holding samples leaks those references, and
  the sender loses those slots. That's tolerable today, because a failed node
  stops the whole dataflow. Supervision with restarts (milestone 7) has to
  address it, e.g. by having the daemon track outstanding references per
  receiver.
- A message queued in a socket just as its receiver exits is never released.
  This is rare; same fix.
- Memory per node is bounded by 32 × its largest message, all in RAM (tmpfs).
- Linux only (`/dev/shm`, `/proc`).
