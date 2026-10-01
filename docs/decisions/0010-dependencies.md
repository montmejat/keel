# 0010: Build the core by hand, use crates at the edges

**Status:** Accepted, 2026-10-01 (proposed 2026-09-24)

## Context
keel is for learning how a middleware works, so writing parts by hand is
often the point. Not everything is worth rewriting, though: a YAML parser or
a SHA-256 teaches little about middleware.

## Decision
Write by hand whatever *is* the middleware:
- transport (shared memory, reference counting, and TCP framing later),
- control protocol,
- scheduling, supervision and lifecycle,
- the bundle format and deployment protocol,
- provisioning logic.

Use established crates for the generic building blocks:
- parsing (serde, a YAML parser),
- system call bindings (`libc`),
- compression, if bundles ever need it (e.g. `zstd`). Hashing was meant to
  be `sha2` too, but SHA-256 ended up written by hand
  ([0016](0016-packaging-and-deployment.md)).
- TUI rendering (`ratatui`),
- and later, if needed, async I/O and SSH.

Rule of thumb: if the crate would *be* the layer, write it. If it's a tool
the layer uses, take the crate.

## Consequences
- Today's dependencies: `libc` (node crate); `libc`, `serde`, `serde_json`
  and `serde_yaml` (daemon); `clap` and `ratatui` (CLI).
- `serde_yaml` is deprecated upstream. Replace it (e.g. `serde_yml`, or TOML)
  before it causes trouble.
- Each new dependency is noted here, in this file's Consequences.
