# The dataflow file

A dataflow is a YAML file: the nodes, what feeds each input, and where and
how each node runs. Paths are relative to the file.

```yaml
machines:                              # optional: then every node names one
  robot: 127.0.0.1:7401
nodes:
  - id: controller
    machine: robot
    path: ../target/release/controller # or `build: controller`
    args: ["--target", "1.0"]
    rt: { priority: 80, cpus: [3] }    # SCHED_FIFO 80, pinned to CPU 3
    phase_us: 100                      # tick 100 µs into each period
    restart: on-failure
    watchdog_ms: 500
    inputs:
      state: joints/state              # <input>: <node>/<output>
      camera: { source: camera/frames, keep: latest }
```

## Fields

| field | meaning | default |
|---|---|---|
| `machines` | name → daemon address, for a dataflow across machines ([machines](machines.md)) | one machine |
| `id` | letters, digits, `_`, `-`; unique | required |
| `machine` | where the node runs: a key of `machines` | required if `machines` is set |
| `path` | an executable already on the node's machine | `path` or `build` |
| `build` | a binary of the dataflow's cargo workspace, built and shipped by keel ([deployment](deployment.md)) | `path` or `build` |
| `args` | command-line arguments | none |
| `inputs` | `<input>: <node>/<output>`, or `{ source: <node>/<output>, keep: latest }` | none |
| `keep` | `all`: every message, in order, the sender waits when the receiver is behind. `latest`: only the newest, the sender never waits (a recorder, a viewer, a controller) | `all` |
| `rt.priority` | SCHED_FIFO priority, 1-99 | normal scheduling |
| `rt.cpus` | CPUs the node may run on | any |
| `phase_us` | where in their period the node's periodic loops tick ([0029](decisions/0029-phases.md)) | 0 |
| `restart` | `never`, `on-failure` (also after a watchdog kill), `always` | `never` |
| `max_restarts` | restarts before giving up on the node and the dataflow; backoff 100 ms → 5 s | 5 |
| `watchdog_ms` | kill the node (then maybe restart it) after this long without taking or sending a message while not waiting | off |

Any `rt:`, even `rt: {}`, also makes the node lock its memory and sets its
timer slack to 1 ns, so its timed wake-ups are on time (the default slack is
50 µs). Real-time priority needs the privilege: an rtprio limit (e.g.
`/etc/security/limits.d/keel.conf` with `<user> - rtprio 95`; the service
`keel provision` installs has `LimitRTPRIO=95`) or `CAP_SYS_NICE`. Without
it the daemon says so, and the rest applies.

## Lifecycle

- The daemon starts nodes only once every node has registered: no message is
  lost at startup.
- A node gets `Stop` once all its upstream nodes have exited and it has
  received everything they sent.
- If a node fails and won't be restarted, the daemon stops the dataflow.
- Stopping drains the dataflow from its sources down; nodes that ignore
  `Stop` get SIGTERM, then SIGKILL.
- `keel update <dataflow>` replaces the nodes whose binary or settings
  changed in a running dataflow, one at a time
  ([0019](decisions/0019-lifecycle.md)). `examples/lifecycle.yml` shows
  restarts and the watchdog, on purpose.
- Periodic loops tick on multiples of their period on the machine's clock:
  loops of one period are in phase whenever they started.
