# Observability

The tools are clients of the daemons' control API
([0007](decisions/0007-tools-are-control-plane-clients.md),
[0011](decisions/0011-control-api.md)): they see a dataflow on one machine
or, through the coordinator, on all of them.

## Looking at a running dataflow

| command | shows |
|---|---|
| `keel top` | live view: nodes, links, latencies, logs, traces. `↑↓` select, `f` filter logs, `g` graph, `s` stop, `q` quit |
| `keel ps` | running dataflows |
| `keel logs [-f] [node]` | logs, all nodes or one |
| `keel stop` | graceful stop, as Ctrl-C in `keel run` |
| `keel trace [--export t.json]` | latencies and traces; the export opens in ui.perfetto.dev |

## Where the time goes

Every message is traced ([0014](decisions/0014-tracing-and-clocks.md)): its
header carries its span, the trace it belongs to, the message that caused
it, when it was published, and its stamp. Recording that costs a clock read
and a few atomic adds. `keel trace` shows, per input, the latency (published
→ taken) and processing time (taken → released) as percentiles, then the
latest sampled traces: a message followed through every hop and every node
it caused, across machines too.

```
camera/frames@robot #11: 7.63s end to end
  → detector/frames@base  7.49s  [send 6.51s, daemon 680ms, network 302ms, route 1.30ms, wake 42.5µs]  then processing 140µs
    detector/brightness #11
      → recorder/brightness@robot  132ms  [send 18.2µs, daemon 34.8µs, network 130ms, route 86.9µs, wake 2.01ms]  then processing 11.8µs
```

A Raspberry Pi on Wi-Fi sending 6 MB frames to a laptop: the frame waited
6.5 s in the camera's queue before its daemon could send it.

## Recording and replay

`keel-recorder` is a node that writes its inputs to a file naming the
dataflow and deployment that produced them
([0017](decisions/0017-recording-and-replay.md)). Each record keeps the
message's span, publish time and stamp.

| command | does |
|---|---|
| `keel recording <file>` | channels, message counts, sizes, duration |
| `keel replay <file> <dataflow> [--speed 2]` | runs the dataflow with its recorded source nodes replaced by the recording: the rest can't tell. Stamps are handed on, so controllers behave the same at any speed |
| `keel export <file> <dir> [--channel c] [--from s] [--to s]` | one file per message, and `index.csv` with times, spans and stamps |

```sh
keel run examples/pipeline-recorded.yml                 # Ctrl-C to stop
keel recording ~/.local/share/keel/recordings/<file>
keel replay <file> examples/pipeline.yml --speed 2      # the camera, replayed
keel export <file> dataset/ --channel brightness --from 10 --to 20
```

Inputs with `keep: latest` let a slow disk skip frames instead of slowing
the camera (the pipeline's 6 MB frames at 30 Hz are ~180 MB/s).

## A flight recorder

With `--last <seconds>`, the recorder keeps only the last seconds, in memory,
and when a node fails the daemon saves them as a recording that says which
node failed and how ([0022](decisions/0022-flight-recorder.md)). Replaying
it runs the failure again:

```sh
keel run examples/flight-recorder.yml
#   [daemon] `blackbox` held the 2.5s before `flaky` failed, 252 messages: saved to ...
keel replay <file> examples/flight-recorder.yml         # `flaky` crashes again, at the same count
```
