# 0003: Payloads are raw bytes for now

**Status:** Accepted, 2026-09-24

## Context
Middlewares usually carry typed messages (ROS IDL, Arrow, protobuf). That's a
large topic in itself and independent of the layers keel is about.

## Decision
A message is a byte buffer. Serialisation is up to the nodes.

## Consequences
- The transport only has to move buffers, which keeps zero-copy simple.
- No schema checks between connected nodes. Revisit if tooling (e.g.
  inspecting messages in the TUI) needs types.
