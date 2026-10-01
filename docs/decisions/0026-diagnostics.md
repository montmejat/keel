# 0026: Diagnostics: checks read from the system, answered by each daemon

**Status:** Proposed, 2026-10-02

## Context
Whether a dataflow behaves depends on the machine under it: a real-time
node on a kernel that can't preempt, without the right to its priority, on
a CPU that slows itself down, is not real-time. keel found these one by one
on the Pi. A robot's machine should be able to say what's wrong with it
before anything runs.

## Decision
- **`keel doctor`** lists checks, each `ok`, `warn` (works, at a cost) or
  `fail` (something keel offers won't work), with what was found and, when
  not `ok`, what it costs and what to do. It exits non-zero on a `fail`.
- **Checks read, they don't measure**: kernel preemption, the real-time
  priority and locked-memory limits, real-time throttling, the CPU
  governor, isolated CPUs, the clock source, swap, shared memory, the token
  and the store, from `/proc`, `/sys` and the process's limits. It takes no
  time and disturbs nothing running.
- **A daemon answers for its machine** (`Diagnose` on the wire), with the
  limits its nodes would inherit, which are the ones that matter. `keel
  doctor <dataflow>` asks every machine of the dataflow. It's the only way
  to look at a machine that is only keel ([0023](0023-process-one.md)).
- **It reports, it doesn't fix.** Fixes are provisioning's
  ([0018](0018-provisioning.md)); the messages point there.

## Consequences
- Tried on the laptop (real-time priority not allowed: a `fail`) and on a
  QEMU machine booted from `keel image` (root, so allowed).
- Reading isn't proof: a machine that passes can still have long latencies
  (firmware, a driver). Measuring is the jitter benchmark's job; running it
  from here would be the next step.
- A daemon older than this doesn't know the request and answers with an
  error: the Pi needs provisioning again.
- Nothing about the dataflow itself is checked (is a node's CPU isolated,
  does its machine have enough cores): only the machine.
