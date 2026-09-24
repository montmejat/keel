# 0002: Rust only

**Status:** Accepted, 2026-09-24

## Context
The proof of concept shipped a C ABI (`keel.h`) and a C++ wrapper so nodes
could be written in C++. That doubles the API surface, and every transport
change has to cross an FFI boundary.

## Decision
Nodes, daemon, coordinator and tools are all Rust. The C ABI, C++ header and
C++ example were removed.

## Consequences
- Zero-copy gets simpler: sender and receiver share Rust types and layouts,
  with no ABI to keep stable.
- Changes to the node API are just refactors.
- Adding another language later means designing a C ABI deliberately, on top
  of the stabilised transport.
