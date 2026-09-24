# 0001: A learning project covering the whole stack, minimally

**Status:** Accepted, 2026-09-24

## Context
keel began as a proof of concept: a daemon that spawns nodes and routes
messages. The goal is to learn how a whole middleware stack fits together,
not to support real robots.

## Decision
keel covers every layer: transport, runtime, lifecycle, coordination,
packaging, deployment, provisioning and tooling. Each layer gets the smallest
implementation that works end-to-end before any layer gets polished.

## Consequences
- Breadth first: a crude layer that connects to its neighbours beats a
  polished one that doesn't.
- Robotics features (drivers, ROS compatibility, message IDLs) are out of
  scope unless a layer needs them.
- Architecture and boundaries between layers matter more than features.
