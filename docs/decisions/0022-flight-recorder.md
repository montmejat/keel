# 0022: Flight recorder: the last seconds in shared memory, saved by the daemon on a failure

**Status:** Accepted, 2026-10-01 (proposed 2026-10-01)

## Context
Recording ([0017](0017-recording-and-replay.md)) has to be decided before
the interesting thing happens, and writes everything to disk. A robot fails
when nobody is recording. What's wanted is the last seconds before a
failure, always, at no cost to the disk, as something `keel replay` can run
again.

## Decision
- **Still an ordinary node:** `keel-recorder --last <seconds>`. The
  dataflow says which inputs it gets and with which policy, as for any
  recorder: keel doesn't subscribe it to everything by itself.
- **The ring is files in shared memory.** The recorder writes short
  recordings (segments, a quarter of the time kept, at most 1 s) one after
  the other under the dataflow's shared-memory directory, each record
  flushed at once, and deletes those too old. Shared memory is RAM: nothing
  touches the disk. And files there outlive processes: what's held survives
  the recorder being killed.
- **The daemon saves, not the recorder.** When a node fails (a failed exit,
  a watchdog kill; not one it was told to stop, or killed because another
  failed), the daemon joins every flight recorder's segments into one
  recording in `recordings/`, named after the dataflow, the time and the
  node. It works whatever happens next: the node restarting, or the whole
  dataflow being killed, recorder included.
- **The recording says why it exists:** its header names the node that
  failed, how, and when (`keel recording` shows it). Otherwise it's a
  recording like any other: `keel replay` and `keel export` take it.
- **Saving happens on the side** (a thread), so a restart isn't held up by
  the disk; the dataflow's shared memory is kept until saves finish.

## Consequences
- Tried on `examples/flight-recorder.yml`: each crash leaves a recording of
  the 2.5 s before it, and replaying one crashes the same node at the same
  count.
- The memory used is the data rate times the seconds kept, with no cap:
  2 s of the camera example's frames is 360 MB. `keep: latest` on big
  inputs, or fewer seconds.
- What's kept overshoots by up to a quarter (one segment), and includes the
  few messages sent between the failure and the daemon noticing (it looks
  every 20 ms).
- A message the failing node received that the recorder hadn't yet written
  would be missing. Not seen: the recorder has those 20 ms to write it.
- Replay only replaces source nodes (0017), so reproducing a failure needs
  the sources' outputs among the recorder's inputs.
- Each machine saves its own recorders, when one of its own nodes fails. A
  failure on another machine saves nothing here; nor does losing power,
  which takes the RAM with it.
- Two failures of one node within a second overwrite each other's file.
