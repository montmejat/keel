# 0007: CLI and TUI are clients of the control API

**Status:** Proposed, 2026-09-24

## Context
keel should come with good terminal tools: a live view of a running dataflow
(nodes, states, message rates, logs), plus deploy and provision commands.

## Decision
- One `keel` binary with subcommands (`run`, `top`, `deploy`, `provision`, ...).
  Interactive views use `ratatui`.
- Tools talk to the coordinator and daemons only through the control-plane API
  ([0004](0004-data-and-control-planes.md)). No tool reads a daemon's
  internals or shared memory directly.
- The control API is designed alongside the first TUI (milestone 3), so the
  tool shapes the API from the start rather than being bolted on.

## Consequences
- Anything the TUI shows has to be exposed by the API, which keeps the API honest.
- Nodes must report metrics to their daemon (see 0004).
- Logs must be captured by the daemon (piping node stdout/stderr) rather than
  inherited, as they are today.
