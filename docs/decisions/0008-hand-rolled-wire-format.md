# 0008: Keep the hand-rolled wire format, for now

**Status:** Proposed, 2026-09-24

## Context
Daemon and nodes talk through length-prefixed frames written by hand
(`crates/keel/src/protocol.rs`), with no serde and no dependencies.

## Decision
Keep it for node-to-daemon control messages: few message types, one machine,
both sides built from the same source.

Revisit when the control plane crosses machines and versions (coordinator ↔
daemon, CLI ↔ daemon). That's when versioning and schema evolution matter,
and something like `postcard` + serde probably pays off.

## Consequences
- Every new message means writing tag, encode and decode by hand. That's
  acceptable at ~5 messages and painful at ~30.
