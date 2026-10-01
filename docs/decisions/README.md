# Decision records

One file per decision: the context, what was decided, and what it costs.
**Accepted** means agreed; **Proposed** means it's waiting for a review.
Superseded records stay, with a link to what replaced them.

| # | Decision | Status |
|---|---|---|
| [0001](0001-learning-project-whole-stack.md) | A learning project covering the whole stack, minimally | Accepted |
| [0002](0002-rust-only.md) | Rust only | Accepted |
| [0003](0003-raw-byte-payloads.md) | Payloads are raw bytes for now | Accepted |
| [0004](0004-data-and-control-planes.md) | Separate data and control planes | Proposed |
| [0005](0005-shared-memory-transport.md) | Zero-copy local transport over shared memory | Proposed |
| [0006](0006-coordinator-and-daemons.md) | One coordinator, one daemon per machine | Proposed |
| [0007](0007-tools-are-control-plane-clients.md) | CLI and TUI are clients of the control API | Proposed |
| [0008](0008-hand-rolled-wire-format.md) | Keep the hand-rolled wire format, for now | Proposed |
| [0009](0009-measure-before-optimizing.md) | Benchmark before each performance change | Accepted |
| [0010](0010-dependencies.md) | Build the core by hand, use crates at the edges | Proposed |
| [0011](0011-control-api.md) | Control API: JSON lines over a Unix socket | Proposed |
| [0012](0012-daemon-as-init.md) | The daemon is a small init for its nodes | Proposed |
| [0013](0013-data-between-machines.md) | Data between machines: TCP to the peer daemon, into its shared memory | Proposed |
| [0014](0014-tracing-and-clocks.md) | Tracing: timestamps at every hop, clocks aligned by keel | Accepted |
| [0015](0015-realtime-data-plane.md) | Real-time data plane: nodes talk directly, the daemon only sets up | Proposed |
| [0016](0016-packaging-and-deployment.md) | Packaging and deployment: reproducible static binaries, stored by hash | Proposed |
| [0017](0017-recording-and-replay.md) | Recording and replay: a recorder node, a flat file, replay in place | Proposed |
| [0018](0018-provisioning.md) | Provisioning over SSH, a systemd service, and a shared token | Proposed |
| [0019](0019-lifecycle.md) | Lifecycle: restart policies, a watchdog, rolling updates | Proposed |
