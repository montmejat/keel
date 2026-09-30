# 0017: Recording and replay: a recorder node, a flat file, replay in place

**Status:** Proposed, 2026-09-30

## Context
Robots produce data worth keeping: to replay a scene through a changed
algorithm, or to turn into a dataset. That data is only useful if it says
which software produced it, and a replay is only useful if the nodes being
tested can't tell it from the real thing.

## Decision
- **The recorder is an ordinary node** (`keel-recorder`, in the workspace):
  it records every input it's given. Since milestone 6 the daemon isn't on
  the data path, so recording is the job of a node, placed like any other
  and reading payloads zero-copy. Nodes gained `args:` in the dataflow (the
  recorder takes an output path).
- **One flat file:** a magic line, a JSON header (dataflow, deployment id,
  channels as `input` ← `node/output`), then records `[len][channel][span]
  [published time][payload]`. The deployment id ties data to the exact
  binaries that produced it ([0016](0016-packaging-and-deployment.md)); the
  span ties a record to its trace ([0014](0014-tracing-and-clocks.md)). No
  index: replay and export read in order. A recorder killed mid-write leaves
  a readable file whose last record is dropped.
- **Replay in place:** `keel replay <file> <dataflow>` runs the dataflow on
  this machine with each recorded *source* node (one without inputs)
  replaced by a replay node that publishes the recorded messages on the same
  outputs, with the recorded spacing (`--speed`, 0 for as fast as
  possible). Downstream nodes run unchanged and can't tell. The replay node
  is `keel` itself (`keel replay-node`), so there's nothing extra to build.
- **Datasets:** `keel export <file> <dir>` writes each message to
  `<channel>/<n>.bin` plus `index.csv` (channel, source, time, span, size),
  optionally one channel and a time window. Payloads stay raw bytes
  ([0003](0003-raw-byte-payloads.md)): decoding is the user's business.

## Consequences
- A slow disk slows the recorder, which with `keep: all` slows the sender
  (backpressure). `keep: latest` on big inputs trades completeness for
  never holding up the robot.
- Recording several machines means a recorder per machine and one file
  each. Their times are each machine's clock, converted for data from
  elsewhere; merging files is left for later.
- Replays run on one machine: the point is re-running an algorithm on data
  a robot recorded, on a laptop.
- Replaying a node that has inputs (e.g. to test a sink against a recorded
  middle stage) isn't supported: only sources are replaced.
