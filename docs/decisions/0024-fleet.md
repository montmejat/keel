# 0024: Fleet: each robot is a deployment of its own, the fleet acts on several

**Status:** Accepted, 2026-10-01 (proposed 2026-10-01)

## Context
Everything so far is one robot: one dataflow on its machines. With several
robots running the same software, the questions change: which robot runs
what, how does new code reach them without stopping them all at once, and
how is it tried on one before the others. Machines that are only keel
([0023](0023-process-one.md)) make robots cheap enough to try this on a
laptop.

## Decision
- **A fleet file** names the dataflow and, for each robot, the address of
  each of the dataflow's machines. The addresses in the dataflow's own
  `machines:` are placeholders.
- **A robot is a deployment name**: `<dataflow>-<robot>`. So each robot has
  what a dataflow already has: a history, a rollback, branches
  ([0016](0016-packaging-and-deployment.md), [0020](0020-branches.md)), and
  its own coordinator when it runs. Nothing new in the daemons or the
  coordinator: a fleet is a loop over robots, in the CLI.
- **Commands:**
  - `keel fleet run` deploys to each robot and runs one coordinator per
    robot (child processes, their logs prefixed with the robot).
  - `keel fleet status`: one line per robot: state, code, deployment, nodes
    running, restarts, and the worst input's 99th-percentile latency.
  - `keel fleet update` builds the dataflow as it is now and rolls it into
    the running robots, one robot after the other, each node by node
    ([0019](0019-lifecycle.md)). `--only <robot>` tries it on some first.
  - `keel fleet rollback` puts robots back on their previous deployment,
    rolling it in if they're running.
- **"Code" is a hash of the binaries alone.** Deployment ids differ between
  robots (their addresses are part of the dataflow), so the status shows a
  second, shorter name that is the same wherever the same binaries run.
- **A deployment's id now includes its name**, so that one dataflow
  deployed under two names is two deployments. (Found here: a robot at the
  address the dataflow file gave had the same id as that file deployed by
  hand, and looking it up by id was ambiguous.)

## Consequences
- Tried on three QEMU machines: new code on robot-3 while 1 and 2 kept the
  old, then on the others, then robot-2 back; no robot stopped.
- Comparing is left to the reader: the status puts robots side by side,
  nothing decides whether the new code is better or stops a rollout that
  goes wrong.
- The fleet is driven from one machine, which holds the histories and runs
  every coordinator. A robot can't be updated from elsewhere, and the
  coordinators die with that machine (their robots then stop).
- `keel fleet update` refuses a changed dataflow, like `keel update`.
- One coordinator process per robot: fine for a handful, not for hundreds.
- Existing deployments keep their ids; deploying unchanged code once more
  gives a new id, since the name is now in it.
