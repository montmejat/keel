# 0004: Separate data and control planes

**Status:** Accepted, 2026-10-01 (proposed 2026-09-24)

## Context
Today the daemon does everything over one socket per node: registration,
lifecycle (`Ready`, `Stop`) and every message payload. So the daemon is on the
hot path, and every byte is copied twice.

## Decision
- **Data plane:** node-to-node payloads, never through the daemon once
  shared memory lands ([0005](0005-shared-memory-transport.md)).
- **Control plane:** registration, lifecycle, routing setup, state, metrics,
  deployment. Goes through the daemon and can be simple and slow.

## Consequences
- The daemon's throughput stops mattering; its correctness does.
- The daemon no longer sees payloads, but it still forwards every
  descriptor, so it counts messages and bytes per output itself. If
  descriptors ever go directly node to node, nodes will have to report metrics.
- Every higher layer (coordinator, deployment, TUI) talks to the control plane
  only.
