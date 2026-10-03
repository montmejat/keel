# Deployment

A node with `build: <cargo binary>` instead of `path:` is built by keel as a
reproducible static binary for its machine (x86_64 or aarch64, musl), named
by its SHA-256, and sent to that machine's daemon only if the daemon doesn't
have it yet ([0016](decisions/0016-packaging-and-deployment.md)). Each
deployment is recorded: a dataflow, a binary per node, the compiler.

Cross-building needs the targets: `rustup target add
aarch64-unknown-linux-musl x86_64-unknown-linux-musl`.

## Commands

| command | does |
|---|---|
| `keel deploy <dataflow>` | build and ship, don't run |
| `keel history [name]` | deployments, `*` the current one |
| `keel start <name>[@branch]` | run the current deployment (of a branch) |
| `keel rollback <name>` | back to the previous deployment |
| `keel update <dataflow>` | new code into a running dataflow, node by node |
| `keel gc --keep 3` | forget older deployments, delete unused binaries |
| `keel diff <a> <b>` | node by node: same, settings, binary, both |

## Branches

A deployed dataflow has branches, to try something without losing what
works ([0020](decisions/0020-branches.md)). Deployments land on the branch
you're on, and `keel start` runs its newest.

```sh
keel branch pipeline-two-machines planner --new   # a branch from the current deployment
keel deploy examples/pipeline-two-machines.yml    # lands on `planner`
keel diff pipeline-two-machines@main pipeline-two-machines
keel branch pipeline-two-machines main            # back to main (no branch name: list them)
keel start pipeline-two-machines@planner          # run a branch without switching to it
keel merge pipeline-two-machines planner          # main takes what planner has
```

## A fleet

Several robots running one dataflow, each a deployment of its own
([0024](decisions/0024-fleet.md)). The fleet file names the dataflow and each
robot's machine addresses; new code goes to some robots first, without
stopping any.

```yaml
dataflow: pipeline-image.yml
robots:
  robot-1: { robot: 127.0.0.1:7411 }
  robot-2: { robot: 127.0.0.1:7412 }
  robot-3: { robot: 127.0.0.1:7413 }
```

```sh
examples/image/boot.sh 3                                # three QEMU robots
keel fleet run examples/fleet.yml                       # deploy to all, run them
keel fleet update examples/fleet.yml --only robot-3     # after changing a node
keel fleet status examples/fleet.yml
keel fleet update examples/fleet.yml                    # the others
keel fleet rollback examples/fleet.yml --only robot-2   # or back
```

```
ROBOT        STATE     CODE      DEPLOYMENT      DEPLOYED  NODES  RESTARTS    LAT p99
robot-1      running   c99cf132  e301eaccd818     29s ago    3/3         0    204.8µs
robot-2      running   c99cf132  9079a98083b2     28s ago    3/3         0    221.2µs
robot-3      running   b05710dd  5bd0660eca87      8s ago    3/3         0    237.6µs
```

`CODE` is the same on robots running the same binaries, whatever their
deployment ids.
