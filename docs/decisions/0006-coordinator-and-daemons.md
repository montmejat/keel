# 0006: One coordinator, one daemon per machine

**Status:** Proposed, 2026-09-24

## Context
Multi-machine is in scope. Something has to decide which node runs where,
and something on each machine has to run it.

## Decision
- **Daemon** (one per machine): spawns and supervises local nodes, sets up
  local transports, owns the local bundle cache.
- **Coordinator** (one per deployment): reads the dataflow, assigns nodes to
  machines, tells each daemon what to run and which remote peers to connect
  to. It isn't on the data path.
- The single-machine case is a coordinator and a daemon in one process, so
  there's only one code path.

## Consequences
- The daemon library needs a clean API the coordinator can drive, which is
  why it was split into a library and a binary in milestone 1.
- Node placement is explicit (a `machine:` field in the dataflow) until there's
  a reason for anything smarter.
- The coordinator is a single point of failure. That's fine for a learning
  project; it's noted and not solved.
- Multi-machine tests run in containers on one host.
