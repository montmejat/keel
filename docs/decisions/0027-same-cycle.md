# 0027: Closing a control loop within its cycle, spinning while it closes

**Status:** Proposed, 2026-10-03

## Context
[0021](0021-control-layer.md) left a known cost: a bus master or simulation
ticks, publishes its state, and only looks at commands at the next tick, so
a command waits about a period for it. Traced on `control-sim.yml` at
1 kHz: the controller answers in 14 µs, then its command waits 987 µs. The
hop between nodes is about 1% of the loop; the rest is phase.

Spinning (`Node::set_spin`, watching the bell instead of sleeping on its
futex) was measured first, one hop one way at 1 kHz on the laptop
(`examples/hop.yml`, no isolation, powersave):

| | p50 | p99 | p99.9 |
|---|---:|---:|---:|
| sleeping (futex) | 6.01 µs | 15.6 µs | 158 µs |
| spinning | 519 ns | 1.61 µs | 4.69 µs |
| the machine, keel out of the way, between cores | 170 ns | 1.23 µs | 1.65 µs |
| the machine, between hyperthreads of one core | 84 ns | 133 ns | 557 ns |

Worth having, but on its own it shortens a 1 ms loop by microseconds.

## Decision
- **A node that publishes a measurement waits for its answer within the
  cycle.** `keel-sim` and `keel-can` publish the state, wait up to
  `--deadline-us` (300 by default) for the command that answers it, and act
  on it before the tick ends: the simulation steps with it, the bus master
  sends it. `--cycle next` keeps the old behaviour, a constant one-period
  delay, for controllers that can't answer within the deadline.
- **The answer is recognised by its trace context**: a controller that sends
  while it holds the state names it as parent. `Node::last_sent_span` gives
  the span to look for. No new field on the wire.
- **A late answer doesn't stretch the cycle**: the joints keep the previous
  command, the late one is used at the next tick, and the misses are logged
  once a second.
- **The node API gets `next_event_timeout`**, and spinning applies to it:
  a node waiting for an answer it knows is microseconds away spins only for
  that window, not for the whole period. That's where spinning pays.
- The shared piece lives in `keel_control::cycle`, above keel: keel only
  gained the timeout and `last_sent_span`.

## Consequences
- Measured, `control-sim.yml` at 1 kHz on the laptop, from state published
  to command applied (`keel trace`, end to end):

  | | state → command applied |
  |---|---:|
  | next cycle (before) | ~1000 µs |
  | same cycle | ~21 µs |
  | same cycle, the simulation spinning in its window | ~15 µs |
  | same cycle, both spinning, each pinned to a core | ~1.3 µs |

  The last one keeps the controller's CPU busy all the time: it can't know
  when the next state comes. Knowing that is phasing (a shared epoch, the
  next step), which would let it wake just before.
- A controller that sends after dropping the state, or that isn't reacting
  to states at all, never answers in time: use `--cycle next` with it, or
  have it send with `send_caused_by`.
- `keel-can` not yet tried on a virtual bus here (no CAN interface on the
  laptop without root). Its stand-in drive, `keel-can-motor`, applies
  commands on its own tick, so the CAN example still has the drive's phase
  in it; a real drive applies a command when it arrives.
- Drives that answer each command with their state (Damiao, in MIT mode)
  report a state taken when the previous command arrived. Closing the loop
  on a fresh state then needs a state request just before the tick, an
  offset within the cycle: phasing again.
- Stepping the simulation only when the answer comes (lockstep) is this
  with no deadline and no timer, but controllers take their time steps from
  the wall clock today. It waits for a time stamp in the message header.
