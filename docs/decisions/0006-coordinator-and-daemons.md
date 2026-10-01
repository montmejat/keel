# 0006: One coordinator, one daemon per machine

**Status:** Accepted, 2026-10-01 (proposed 2026-09-24). Implemented in milestone 4.

## Context
Multi-machine is in scope. Something has to decide which node runs where,
and something on each machine has to run it.

## Decision
- **Placement is explicit.** A dataflow that lists `machines:` (name →
  daemon address) puts each node on one with `machine:`. Without `machines:`,
  everything runs on this machine, as before.
- **`keel daemon`**, one per machine, long-lived. It runs its machine's share
  of one dataflow at a time, and refuses a second one while busy.
- **The coordinator** is `keel run` on a multi-machine dataflow. It sends each
  daemon the whole dataflow and its machine name (`spawn`), releases every
  node once all machines report their nodes registered (`start`), relays
  stop requests from any machine to all of them (`stop`), and aborts
  everything when a node fails or a machine is lost (`abort`). It exits when
  every daemon reports its nodes finished. It never touches data.
- **One code path.** A *session* runs one machine's share of a dataflow, and
  a single-machine `keel run` is a session driven in-process. The same code
  runs whether there's one machine or ten.
- Coordinator ↔ daemon messages are JSON lines over TCP, like the control API
  ([0011](0011-control-api.md)). Node logs are forwarded to the coordinator,
  so `keel run` shows everything, prefixed `[node@machine]`.

## Consequences
- The global startup barrier and ordered stop from single-machine keel still
  hold across machines.
- The coordinator is a single point of failure: if it dies, daemons abort
  their share. That's fine for a learning project; noted, not solved.
- Binaries must already exist on each machine at the same path as on the
  coordinator's (node paths resolve against the dataflow's directory). That's
  what packaging and deployment replace ([0016](0016-packaging-and-deployment.md),
  for `build:` nodes). The containers example still mounts
  the repository at the same path.
- **No authentication.** Anyone who can reach a daemon's port can make it
  run any program. Daemons listen on `127.0.0.1` unless told otherwise, and
  should only listen on trusted networks. Authentication belongs with
  deployment.
- `keel top`/`ps`/`logs` show one machine each (the daemon they talk to).
  A cluster-wide view would go through the coordinator.
