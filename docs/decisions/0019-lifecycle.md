# 0019: Lifecycle: restart policies, a watchdog, rolling updates

**Status:** Accepted, 2026-10-01 (proposed 2026-10-01)

## Context
Until now a node that failed stopped the whole dataflow, a node stuck in a
loop went unnoticed, and changing a node's code meant stopping everything.
A robot should survive a crashing node, notice a hung one, and take a fix
without a full restart.

## Decision
- **Restart policies per node:** `restart: never` (the default: a failure
  stops the dataflow, as before), `on-failure` or `always`, with
  `max_restarts` (default 5) and a backoff from 100 ms doubling to 5 s. A
  node that has run a minute gets its restarts back. A node that exits
  because it was told to stop (its upstream is gone) isn't restarted.
- **Nothing queued is lost.** While a node is down, what's sent to it waits
  in its channels ([0015](0015-realtime-data-plane.md)); a restarted node
  registers into the running dataflow, gets its routes at once, and reads
  them. Its stop flag survives the restart, so a node whose upstream ended
  meanwhile still drains and exits.
- **What a dead node held is given back.** Each node keeps a table in
  shared memory of the samples it holds (`<node>.held`); when it dies, the
  daemon releases them, so its senders don't lose regions for good. A
  restarted sender adopts its previous regions, honouring their reference
  counts. The message a node was processing when it died is lost: the
  lifecycle example loses exactly those, and nothing else.
- **The watchdog:** `watchdog_ms: N` kills a node that has neither taken nor
  sent a message for N ms while not waiting (for input, or for a free
  region under backpressure): nodes say which, in their stats file. It
  counts as a failure, so the restart policy applies.
- **Rolling updates:** `keel update <dataflow>` deploys the new version
  ([0016](0016-packaging-and-deployment.md)), checks the dataflow itself is
  unchanged, and tells the running dataflow (through the coordinator, every
  machine) to switch: nodes whose binary changed are replaced one at a
  time, each with SIGTERM and a restart on the new binary once the previous
  one is back. The others never stop.
- **Exits are handled where processes are reaped**, not when their socket
  closes, so a node about to restart doesn't make its downstream stop.

## Consequences
- A node that wants to finish its current message on update should handle
  SIGTERM; otherwise that message is lost, as on a crash.
- `keel update` refuses a changed topology (nodes, inputs, machines,
  settings): that still needs a stop and a run.
- The watchdog can't tell a node that's legitimately busy for longer than
  its limit from a hung one: the limit says how long is too long.
- Not done: health checks a node defines itself (beyond progress), and
  updating the daemons themselves without stopping (that's `keel provision`
  then a restart).
