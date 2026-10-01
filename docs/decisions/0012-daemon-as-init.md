# 0012: The daemon is a small init for its nodes

**Status:** Accepted, 2026-10-01 (proposed 2026-09-24)

## Context
The daemon starts nodes, so it's responsible for how they end: on Ctrl-C, on
`keel stop`, when a node fails, and when the daemon itself dies.

## Decision
Use what Linux already offers for process supervision:
- **Process groups:** each node gets its own (`setpgid`), so Ctrl-C in the
  terminal reaches only the daemon, which then stops nodes in order.
- **Parent-death signal:** `PR_SET_PDEATHSIG = SIGKILL`, so nodes die if the
  daemon is killed, instead of lingering as orphans.
- **Log capture:** node stdout/stderr go through pipes to the daemon, which
  prefixes each line and keeps it for `keel logs` and `keel top`.
- **Stop drains the dataflow:** sources and nodes on cycles get `Stop` first.
  Every other node gets it once all its upstream nodes have exited, so
  messages in flight are delivered, however long a slow link takes. If
  nothing moves for 5 s (no message delivered, no node gone), every node gets
  `Stop`.
- **Escalation, per node:** `Stop`, then SIGTERM 2 s later, then SIGKILL 3 s
  after that. A node that dies of a SIGTERM we sent counts as a clean exit.
- A second Ctrl-C kills everything at once. A node failing outside a stop
  still kills the rest (no restart policy yet).

## Consequences
- Pure sources such as `camera` never read events, so they never see `Stop`
  and always take the 2 s SIGTERM path. Daemon-generated timer inputs, as in
  dora, would make sources event-driven and let them stop cleanly. That's a
  good candidate for later.
- `PR_SET_PDEATHSIG` is Linux-specific, and fires when the *thread* that spawned
  the node exits. Nodes are spawned from the main thread for that reason.
- Every node's output goes through the daemon: cheap for logs, but a node
  printing heavily costs the daemon CPU.
- This overlaps with what systemd does (cgroups, journald, stop timeouts).
  Delegating to systemd transient units is worth considering when deployment
  lands (milestone 5).
