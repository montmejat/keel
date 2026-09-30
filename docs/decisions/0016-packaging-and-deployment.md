# 0016: Packaging and deployment: reproducible static binaries, stored by hash

**Status:** Proposed, 2026-09-30

## Context
Until now every machine needed the node binaries at the same path as on the
coordinator: the Pi was set up by cross-compiling and `rsync`ing by hand.
Nothing said which build a machine ran, nothing could go back to a previous
one, and nothing ever cleaned up. Dependencies stay cargo-native: crates
wrapping C libraries (`-sys` crates) are out of scope.

## Decision
- **A node is built, not found.** `build: <cargo binary>` instead of
  `path:` in the dataflow. `keel` builds it from the dataflow's cargo
  workspace for the machine it runs on, which the daemon reports
  (`Hello`, e.g. `aarch64-unknown-linux-musl`). `path:` still works for
  binaries that are already in place.
- **Reproducible and static.** `cargo build --release --locked`, for the
  musl target (no dependency on the machine's libc), linked by the
  `rust-lld` that ships with Rust (no cross toolchain), with the workspace,
  target, cargo and rustup directories remapped in the binary and debug info
  stripped. Two builds in different directories give the same bytes. The
  compiler isn't pinned (rustup would download a second toolchain); each
  deployment records `rustc --version` instead.
- **Named by SHA-256**, written by hand (about 100 lines, checked against
  the standard test vectors), unlike what [0010](0010-dependencies.md)
  suggested: part of the "as vanilla as possible" direction of 2026-09-30.
- **A store on every daemon:** `~/.local/share/keel/store/<sha256>`,
  read-only. The coordinator asks which hashes a daemon lacks and sends only
  those, each over its own connection (`BLOB`), checked against its hash
  before it's renamed into place. Deploying unchanged code sends nothing.
- **A deployment** is the dataflow plus each built node's hash; its id is a
  hash of those, so the same code deploys to the same id. The machine that
  runs `keel` records it under `deployments/<name>/<id>.json` and points
  `deployments/<name>/current` at it (symlink, replaced with `rename(2)`).
  Daemons record which hashes each deployment pins (`refs/<id>`).
- **Commands:** `keel run` deploys first when a dataflow has `build:`
  nodes. `keel deploy` builds and ships without running. `keel history`,
  `keel rollback <name> [id]` (the previous one by default), `keel start
  <name|id>` runs a recorded deployment, and `keel gc [--keep N]` forgets
  all but the newest N per name (and the current one), then has each
  machine delete binaries no remaining deployment pins.

## Consequences
- The Pi needs nothing but a `keel daemon`; getting that onto a bare
  machine is provisioning (milestone 10).
- Building needs the target installed on the deploying machine
  (`rustup target add aarch64-unknown-linux-musl`); keel says so if not.
- History lives on the machine deployments were made from. From another
  machine, daemons still have the binaries, but not the list.
- A machine that can't be reached during `gc` keeps its binaries, and the
  deployment is kept so that a later `gc` can finish the job.
- Changing a build setting (e.g. `panic = "abort"`) changes every binary's
  hash, as it should: it's a different program.
- Still no authentication: anyone reaching a daemon can store and run a
  binary. Deployment makes that easier, not worse; it belongs with
  provisioning (keys set up over SSH).
