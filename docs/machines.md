# Machines

A keel machine runs one daemon. The daemon spawns the machine's nodes, hands
them their routes, and forwards messages to the other machines' daemons
([0006](decisions/0006-coordinator-and-daemons.md),
[0013](decisions/0013-data-between-machines.md)).

## Making one

| way | what you get | command |
|---|---|---|
| provision over SSH | keel built for the machine, installed, run by systemd, with real-time limits and a shared token | `keel provision pi` · `keel provision --remove pi` |
| boot an image | a 4.5 MB initramfs: keel as process 1, no distribution, no systemd, no shell | `keel image` |
| by hand | a daemon in a terminal, no token: any connection accepted, trusted networks only | `keel daemon --listen 127.0.0.1:7401` |

- **Provisioning** ([0018](decisions/0018-provisioning.md)) takes a host from
  `~/.ssh/config`, runs from this repository, and needs SSH, systemd, and
  ideally passwordless sudo (for the real-time limits). The token
  authenticates every connection to a daemon but doesn't encrypt: use a VPN
  (WireGuard) on an untrusted network.
- **An image** ([0023](decisions/0023-process-one.md)) mounts what it needs,
  loads a network driver, takes its address from the kernel command line and
  runs the daemon. In QEMU it's up in a second:

  ```sh
  keel image                            # keel.cpio, and the QEMU command to boot it
  examples/image/boot.sh 3              # three of them, on 127.0.0.1:7411-7413
  keel run examples/pipeline-image.yml
  examples/image/boot.sh stop
  ```

- **Containers**: `examples/containers/run.sh` runs each machine in its own
  Podman container.

## A dataflow across machines

List the machines, place each node on one, start a daemon per machine, and
run the dataflow from the machine with the sources. keel builds each node
for its machine's architecture and ships it ([deployment](deployment.md)).

```yaml
machines:
  robot: 127.0.0.1:7401
  base: 127.0.0.1:7402
nodes:
  - id: camera
    machine: robot
    build: camera
  - id: detector
    machine: base
    build: detector
    inputs:
      frames: camera/frames
```

```sh
keel daemon --listen 127.0.0.1:7401 &    # "robot"
keel daemon --listen 127.0.0.1:7402 &    # "base"
keel run examples/pipeline-two-machines.yml
```

`keel run` is then the coordinator: `keel top` shows the whole dataflow
through it (`g` for the graph, links across machines in yellow). Messages
between machines go over TCP, daemon to daemon, into the receiver's shared
memory. keel measures each machine's clock offset (to a few milliseconds
over Wi-Fi) and converts traces and stamps with it
([0014](decisions/0014-tracing-and-clocks.md)). Daemons of different keel
versions may not understand each other: provision them together.

## Is a machine fit for a robot?

`keel doctor` reads what a robot needs from its machine
([0026](decisions/0026-diagnostics.md)): the kernel's preemption, the
real-time and memory-lock limits, real-time throttling, the CPU governor,
isolated cores, the clock source, swap, shared memory, the token and the
store. With a dataflow it asks each of its machines' daemons, so it works on
a machine with no shell. Each check is `ok`, `warn` (works, at a cost) or
`fail`, with what to do; it exits non-zero on a `fail`.

```sh
keel doctor                                 # this machine
keel doctor examples/pipeline-image.yml     # the machines it runs on
```

```
robot (127.0.0.1:7411)
  warn  kernel               7.2.7-200.fc44.x86_64, PREEMPT_DYNAMIC: a real-time node can be woken milliseconds late ...
  ok    real-time priority   running as root
  ok    locked memory        no limit
  warn  isolated CPUs        none: pinned nodes share their core with everything else; `isolcpus=` ...
  ok    store                /root/.local/share/keel, 0 binaries, 0 MiB, in memory: emptied by a reboot
```

It reads, it doesn't measure: the latency benchmarks (`examples/jitter.yml`,
`examples/hop.yml`) do that.
