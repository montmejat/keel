# 0020: Branches: a named list of deployments per dataflow

**Status:** Proposed, 2026-10-01

## Context
Deployments already look like git: binaries stored by hash, a deployment
that names them, a history, a rollback
([0016](0016-packaging-and-deployment.md)). What's missing is a way to try
something next to what works: every deployment of a dataflow lands in the
same history and becomes the current one. The idea of 2026-10-01: a stack
anyone can branch, test something on, and bring back.

## Decision
- **A branch is a list of deployment ids, oldest first**, one file per
  branch (`deployments/<name>/branches/<branch>`), with `head` naming the
  branch that's switched to. `current` stays, and is always that branch's
  newest deployment, so `keel start <name>` and the daemons are unchanged.
- **Not a parent pointer in the deployment.** A deployment's id is a hash of
  its content, so the same code deployed twice, or on two branches, is one
  deployment: it can't say which came before it. The branch's list can.
- **Commands**, without git's names where they're odd:
  - `keel branch <name>` lists; `keel branch <name> <branch>` switches;
    `--new` starts one from the current deployment; `--delete` removes one.
  - `keel deploy`, `run` and `update` add to the branch that's switched to.
    `keel rollback` drops that branch's newest deployment.
  - `name@branch` names a branch's newest deployment wherever a deployment
    is expected (`keel start pipeline@planner`).
  - `keel diff <a> <b>` says, node by node, whether the binary, the settings
    or both differ.
  - `keel merge <name> <branch>` brings the current branch up to another,
    forward only: the other branch must have started from, or passed
    through, where this one is.
- **Existing histories** become `main`, up to their current deployment, the
  first time a branch is made.
- `keel gc` keeps every branch's newest deployment.

## Consequences
- A branch says nothing about sources: those are in git. Deploying from the
  wrong checkout onto a branch is possible, and keel can't tell.
- Two branches that both moved can't be merged. A merge node by node (take
  each node from the side that changed it) is possible, since a deployment
  is a map from node to binary, but the result would match no source tree.
  Left out until it's wanted.
- Branches live where deployments are recorded: on the machine that
  deploys. Sharing them (`keel clone`) needs the daemons to hold the
  history, not only the binaries.
- Not done, and the interesting part: a branch **running beside** the live
  dataflow, with only its changed nodes started, reading the live outputs
  from shared memory and keeping its own outputs to itself. That needs
  nodes with side effects to be marked, branch inputs forced to `keep:
  latest`, and no real-time priority for branch nodes.
