# Decision records

One file per decision: the context, what was decided, and what it costs.
**Accepted** means agreed; **Proposed** means it's waiting for a review.
Superseded records stay, with a link to what replaced them.

| # | Decision | Status |
|---|---|---|
| [0001](0001-learning-project-whole-stack.md) | A learning project covering the whole stack, minimally | Accepted |
| [0002](0002-rust-only.md) | Rust only | Accepted |
| [0003](0003-raw-byte-payloads.md) | Payloads are raw bytes for now | Accepted |
| [0004](0004-data-and-control-planes.md) | Separate data and control planes | Accepted |
| [0005](0005-shared-memory-transport.md) | Zero-copy local transport over shared memory | Accepted |
| [0006](0006-coordinator-and-daemons.md) | One coordinator, one daemon per machine | Accepted |
| [0007](0007-tools-are-control-plane-clients.md) | CLI and TUI are clients of the control API | Accepted |
| [0008](0008-hand-rolled-wire-format.md) | Keep the hand-rolled wire format, for now | Accepted |
| [0009](0009-measure-before-optimizing.md) | Benchmark before each performance change | Accepted |
| [0010](0010-dependencies.md) | Build the core by hand, use crates at the edges | Accepted |
| [0011](0011-control-api.md) | Control API: JSON lines over a Unix socket | Accepted |
| [0012](0012-daemon-as-init.md) | The daemon is a small init for its nodes | Accepted |
| [0013](0013-data-between-machines.md) | Data between machines: TCP to the peer daemon, into its shared memory | Accepted |
| [0014](0014-tracing-and-clocks.md) | Tracing: timestamps at every hop, clocks aligned by keel | Accepted |
| [0015](0015-realtime-data-plane.md) | Real-time data plane: nodes talk directly, the daemon only sets up | Accepted |
| [0016](0016-packaging-and-deployment.md) | Packaging and deployment: reproducible static binaries, stored by hash | Accepted |
| [0017](0017-recording-and-replay.md) | Recording and replay: a recorder node, a flat file, replay in place | Accepted |
| [0018](0018-provisioning.md) | Provisioning over SSH, a systemd service, and a shared token | Accepted |
| [0019](0019-lifecycle.md) | Lifecycle: restart policies, a watchdog, rolling updates | Accepted |
| [0020](0020-branches.md) | Branches: a named list of deployments per dataflow | Accepted |
| [0021](0021-control-layer.md) | Control is a layer above keel, and hardware is a node | Accepted |
| [0022](0022-flight-recorder.md) | Flight recorder: the last seconds in shared memory, saved by the daemon on a failure | Accepted |
| [0023](0023-process-one.md) | keel as process 1: a machine that is a kernel and keel | Accepted |
| [0024](0024-fleet.md) | Fleet: each robot is a deployment of its own, the fleet acts on several | Accepted |
| [0025](0025-microcontroller-nodes.md) | A microcontroller is a node, behind a bridge on a serial link | Accepted |
| [0026](0026-diagnostics.md) | Diagnostics: checks read from the system, answered by each daemon | Proposed |
| [0027](0027-same-cycle.md) | Closing a control loop within its cycle, spinning while it closes | Proposed |
| [0028](0028-stamps.md) | Stamps: every message says what moment it describes | Proposed |
| [0029](0029-phases.md) | Phases: periodic loops tick on the clock's grid | Proposed |
| [0030](0030-agent-tools.md) | An agent looks through the control API, and only looks | Proposed |
