# 0007: CLI and TUI are clients of the control API

**Status:** Accepted, 2026-10-01 (proposed 2026-09-24)

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
  inherited. Done in milestone 3, see [0012](0012-daemon-as-init.md).
- Milestone 3 shipped `keel run | ps | logs [-f] [node] | stop | top`; the
  API is described in [0011](0011-control-api.md).

## The cluster view (milestone 9)
- **Through the coordinator.** A multi-machine `keel run` serves the same
  control API ([0014](0014-tracing-and-clocks.md)) and answers it by asking
  every daemon: status (nodes with their machine and program, links),
  logs, traces. Tools pick the coordinator when several daemons run on one
  host, so `keel top` shows the whole dataflow without being told.
- **Latency per link**, from each receiver's histograms, polled once a
  second with a `trace` request that skips the spans (`summary: true`).
  Links across machines show their clock uncertainty.
- **A drawn graph** (`g`): nodes in columns by depth, each link labelled
  with its rate and median latency, links across machines in yellow,
  links closing a cycle routed underneath. Plain text and braille lines on
  a ratatui canvas.
- The header names the machines and the deployment running; each node shows
  its program, so a recorder (`keel-recorder`) is visible as such.
- Still one view per dataflow: with several dataflows on one set of daemons
  (not supported yet), there would be several coordinators to choose from.
