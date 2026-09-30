# 0013: Data between machines: TCP to the peer daemon, into its shared memory

**Status:** Proposed, 2026-09-24

## Context
Nodes on different machines can't share memory. The node API must not
change: a node shouldn't know, or care, where its peers run.

## Decision
- **One TCP connection per ordered machine pair,** opened by the sending
  daemon before nodes start, carrying `PeerMsg` frames. They use the same
  length-prefixed binary format as the node protocol
  ([0008](0008-hand-rolled-wire-format.md)).
- **Sending side:** when a local node sends on an output with targets
  elsewhere, its daemon copies the payload from shared memory onto each
  machine's connection (once per machine, however many targets there), then
  releases the region as usual.
- **Receiving side:** the daemon writes the payload into a shared-memory pool
  of its own, **named after the remote source node** (`/dev/shm/keel-<pid>/camera.0`
  on `base` for `camera` on `robot`), then delivers it exactly like a local
  message. Receivers can't tell the difference.
- **End of stream in-band:** when a node exits, its daemon sends
  `Closed { node }` on the same connections, after its last data. So
  downstream nodes on other machines get `Stop` only after everything the
  node sent, and draining works across machines.

## Consequences
- Two copies per remote message (shared memory → socket → shared memory) plus
  the kernel's. The benchmark across two daemons on one host: ~58 µs round
  trip for small messages, 5.6 ms for 8 MiB, 1.4–3.6 GB/s.
- Backpressure is natural: a full receiving pool blocks the connection, which
  blocks the sending daemon, which stops reading from the sending node.
- The sending daemon's per-node thread writes to the network, so one slow
  remote machine slows that node's local deliveries too.
- No reconnection: a lost data connection is logged and that machine gets no
  more data from this one.
