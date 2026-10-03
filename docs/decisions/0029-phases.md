# 0029: Phases: periodic loops tick on the clock's grid

**Status:** Proposed, 2026-10-03

## Context
Each periodic loop ticked one period after it started. Two loops of the same
period on a machine don't drift apart (they read the same clock), but their
phase was an accident of start-up: a bus master and its stand-in drive, two
arms' masters, a policy and a camera, each where it happened to start. A
state could be read just before or just after the drive reported it,
changing the delay by a period.

Closing the loop within the cycle ([0027](0027-same-cycle.md)) left another
cost: a controller spinning for its states to answer fast kept a CPU busy
all the time, since it couldn't know when they'd come.

## Decision
- **Ticks fall on multiples of the period** on the machine's monotonic
  clock, plus the node's phase. Loops of one period tick together, a 20 ms
  loop with every 20th tick of a 1 ms one, whenever they started. No epoch
  to share: the clock's zero is one.
- **`phase_us` in the dataflow** moves a node along its period: the daemon
  passes it (`KEEL_PHASE_NS`), `Periodic` uses it. It's one number per node,
  in the file that already says where each node runs.
- **A node can spin around its inputs' schedule**
  (`Node::set_spin_around`): from `spin` before each expected arrival
  (period and the node's phase) to `spin` after, asleep in between.
- **Within a machine.** Clocks across machines are aligned to milliseconds
  ([0014](0014-tracing-and-clocks.md)); phases between them would need PTP.

## Consequences
- The controller of `control-sim.yml`, pinned, the simulation pinned and
  spinning in its window (laptop, 1 kHz):

  | controller | its CPU | state hop p50 | p99 | end to end |
  |---|---:|---:|---:|---:|
  | spinning | 100% | 368 ns | 2.0 µs | ~1.3–2.2 µs |
  | spinning ±30 µs around its states | 6% | 1.1 µs | 7.4 µs | ~3–4 µs |
  | asleep | ~0% | 5.4 µs | 13.8 µs | ~7–13 µs |

- `control-can.yml` puts the stand-in drive's report 100 µs before the bus
  master's tick (`phase_us: 900`). At 50 µs, a fifth of the ticks had no
  fresh state: a node that isn't real-time wakes up to 50 µs late, the
  kernel's default timer slack (`rt: {}` makes it 1 ns). A phase is a
  budget: it has to cover the wake-up's lateness.
- The stand-in drive now applies a command when its frame arrives, as a
  drive does, and reports on its own tick.
- Every `Periodic` changed: the first tick is the next on the grid, not one
  period from start.
- Phases are set by hand. Nothing checks that a node's phase leaves its
  work enough room before the next one's, nor shows phases in `keel top`.
