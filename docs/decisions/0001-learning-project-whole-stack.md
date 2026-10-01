# 0001: A learning project covering the whole stack, minimally

**Status:** Accepted, 2026-09-24. Amended 2026-10-01: real robots are in scope.

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

## Amendment, 2026-10-01
Supporting real robots is no longer out of scope. What a robot needs
(fieldbuses, drivers, simulators, microcontrollers) can be built, and is
being built: see [0021](0021-control-layer.md) and
[0025](0025-microcontroller-nodes.md). The rest stands: every layer, the
smallest version that works end to end, boundaries before features. Still
out of scope unless something needs it: ROS compatibility.
