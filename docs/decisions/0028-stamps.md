# 0028: Stamps: every message says what moment it describes

**Status:** Proposed, 2026-10-03

## Context
A message's header says when keel was handed it (`published_ns`, for
latencies). A controller needs something else: when the data was true. A
PID took its time step as the gap between two states' *arrivals*, so it
integrated too much when a simulation ran slower than real time, too little
in a replay at twice the speed, and the jitter of delivery on hardware.
Stepping a simulation only when its controller answers (lockstep,
[0027](0027-same-cycle.md)) was blocked on this.

## Decision
- **A second time in every message's header**, `stamp_ns`: the moment of
  the world the data describes. In keel, not in the payloads, so every
  message has one, not only control's.
- **Chosen by whoever knows it, inherited by the rest.** A root takes the
  time it's published unless its sender says otherwise
  (`Node::send_stamped`): a bus master stamps when it read the drives, a
  simulation stamps its own time. A message caused by another (`send_with`
  while holding it, `send_caused_by`) inherits its stamp, as it inherits
  its trace: a command carries the stamp of the state it answers.
- **Two clocks on purpose**: `published_ns` is always the machine's, for
  keel's own timing; `stamp_ns` is the world's, real or simulated.
- Across machines it's converted like `published_ns`, into the receiving
  machine's clock.
- **Recordings keep it** (`KEELREC2`, a field more per record), and replays
  hand it on: a replayed state carries the stamp it was recorded with.
  Older recordings aren't read: keel has no users to keep them for.

## Consequences
- `keel-pid` takes `dt` from stamps, and logs once a second of that time.
  `keel-sim --cycle lockstep` exists: 1301 simulated seconds in 6 s on the
  laptop, on the same trajectory as in real time (0.971, 0.990, 0.996 rad
  at 1, 2, 3 s in both).
- The header grows by 8 bytes, within its 64; the wire between daemons by
  8 bytes per message. A daemon of before and one of after can't talk:
  provision them together.
- `keel export` lists each message's stamp in `index.csv`.
- A simulation's stamps aren't the clock's: comparing a stamp with now (a
  message's age) means something only for stamps the clock took. `keel
  trace` doesn't show ages yet.
- A node that combines inputs picks the stamp its output inherits, by
  choosing its cause. Two inputs of different moments (an arm's state and a
  camera frame) are matched by their stamps, by the node: keel only carries
  them.
