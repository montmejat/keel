# Examples

Each file's header says what it shows and what it needs. Run with
`./target/debug/keel run examples/<file>` after `cargo build` (the benchmarks:
`./target/release/keel` after `cargo build --release`).

| dataflow | shows |
|---|---|
| `dataflow.yml` | talker and listener: the smallest dataflow |
| `pipeline.yml` | a 1080p camera at 30 Hz, a detector, and a node logging its brightness reports; runs until stopped |
| `pipeline-recorded.yml` | the same, recorded to disk, for `keel replay` and `keel export` |
| `pipeline-two-machines.yml` | the pipeline over two daemons ([machines](../docs/machines.md)) |
| `pipeline-built.yml` | nodes built and shipped by keel, as a deployment ([deployment](../docs/deployment.md)) |
| `pipeline-image.yml` | on a machine whose only program is keel (`image/boot.sh`) |
| `fleet.yml` | a fleet file: the pipeline on three QEMU robots |
| `lifecycle.yml` | restarts and the watchdog, with nodes that crash and hang on purpose |
| `flight-recorder.yml` | a recorder keeping the last 2 s, saved when a node fails |
| `control-sim.yml` | a PID holding a simulated joint at 1 kHz, the loop closed within its cycle ([control](../docs/control.md)) |
| `control-lockstep.yml` | the same in lockstep, faster than real time |
| `control-can.yml` | the joint behind a virtual CAN bus, a stand-in drive at the other end |
| `control-micro.yml` | the joint on a (pretend) microcontroller |
| `bench.yml` | round-trip latency and throughput, 64 B to 8 MiB |
| `bench-two-machines.yml` | the same across two daemons |
| `jitter.yml`, `jitter-rt.yml` | a 1 kHz loop's lateness, without and with real-time settings |
| `agent-slow.yml`, `agent-stall.yml`, `agent-crash.yml` | a filter that fails in one way each, for an agent to diagnose with `keel-mcp`: `agent/run.sh stall` |
| `hop.yml`, `hop-spin.yml` | one hop, one way, the receiver sleeping or spinning; `hop-floor` measures the machine without keel |

`containers/run.sh` runs the pipeline with each machine in its own Podman
container.
