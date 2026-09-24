# 0005: Zero-copy local transport over shared memory

**Status:** Proposed, 2026-09-24. Details are settled in milestone 2.

## Context
Baseline ([architecture](../architecture.md#baseline-milestone-1)): an 8 MiB
message takes ~29 ms round trip, ~435 MB/s. Every message is copied into the
daemon and out again.

## Decision
On the same machine, the sender writes its payload directly into a
shared-memory buffer, and receivers map the same buffer. Only a small
descriptor (which buffer, offset, length) travels as a notification. Across
machines the payload goes over TCP ([0006](0006-coordinator-and-daemons.md)),
behind the same node API.

Questions for milestone 2:
- Who allocates buffers (sender, daemon) and how they're sized.
- How the sender knows every receiver is done with a buffer, so it can reuse it
  (reference counts in shared memory vs. acknowledgements).
- Notification channel: keep the Unix socket, or a futex/eventfd per queue.
- Slow receivers: block the sender, drop, or keep only the latest message.
- Whether to build this from `memfd` + `mmap` by hand (more learning) or use a
  crate like `iceoryx2` (less learning, more robust). The lean is by hand.

## Consequences
- The node API changes: sending means borrowing a buffer and filling it,
  instead of passing `&[u8]`.
- Crash cleanup gets harder: a node can die while holding buffers.
