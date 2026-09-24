# 0004: Separate data and control planes

**Status:** Proposed, 2026-09-24

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
- Metrics like message rates have to be reported by nodes, since the daemon
  no longer sees the messages.
- Every higher layer (coordinator, deployment, TUI) talks to the control plane
  only.
