# Working on keel

Instructions for any agent changing this repository (Claude Code reads them
through `CLAUDE.md`). The README is what keel is; `docs/architecture.md` is
how it fits together; `docs/decisions/` is why, one record per decision.

## What keel promises

- **The smallest thing that works, at each layer**, built on what Linux
  already ships (shared memory, futexes, SCHED_FIFO, systemd, SSH,
  SocketCAN). The middleware itself is written by hand; established crates
  are used at the edges ([0010](docs/decisions/0010-dependencies.md)). A new
  dependency is noted in that record's Consequences.
- **Rust and Linux only** ([0002](docs/decisions/0002-rust-only.md)).
  Anything heavy lives outside keel, behind a standard interface: motors
  behind CAN, bodies as MJCF, cameras through V4L2, networks as ONNX. The
  simulated robot is [drydock](https://github.com/montmejat/drydock), a
  separate repository, started on its own as a real robot would be.
- **keel runs keel nodes only.** It doesn't supervise arbitrary programs.
- **Numbers are measured.** A performance change comes with a benchmark
  before and after ([0009](docs/decisions/0009-measure-before-optimizing.md));
  every number in the docs comes from a run that says how it was made.

## Decisions

A design decision gets a record in `docs/decisions/`: the next number,
its context, the decision, and what it costs. It's **Proposed** until the
user agrees, then **Accepted**, and it's listed in
`docs/decisions/README.md`. A record that's replaced stays, with a link to
what replaced it.

## Docs live in three places, no more

- **Next to the code**: `docs/*.md` and the decision records.
- **The book**: [keel-book](https://github.com/montmejat/keel-book), for
  guides, tutorials and diagrams.
- **`keel web`**, for anything live.

Don't start a fourth (a wiki, another site). Explainers that only teach
keel, or one example, belong in the book, not in keel's core.

## What must not break

The dataflow file's format, the control API (JSON lines over the Unix
socket, [0011](docs/decisions/0011-control-api.md)), the node wire format,
the CLI's commands and flags, and `keel-control`'s messages. Changing any of
them is a breaking change: say so in the pull request (label `breaking`),
and update the docs and the examples with it.

## Before saying a change is done

```sh
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Then, when the change touches CAN, run the tests that need a bus, on
`vcan0` (the user creates it; it needs sudo):

```sh
sudo ip link add dev vcan0 type vcan && sudo ip link set vcan0 mtu 72 up
cargo test -p keel-control -- --ignored
```

And when it touches control, run it against drydock: `drydock
examples/openarm/robot.yml` in drydock's repository, then `keel run
examples/openarm.yml` here (`docs/control.md`).

CI runs the first two on every pull request. A change is done when CI
passes, the docs say what changed for someone using keel, and the examples
still run.

## How the code is written

- **Designed for what it supports today.** No flags, abstractions or
  fallbacks for features that might come; refactor when one does. Remove
  what isn't used.
- **Plain, short sentences** in comments, docs, logs and messages, written
  for someone who doesn't know the code. Comments say why, not what. Facts
  with numbers ("waited 7.2 ms"), not verdicts.
- **Errors say what's wrong and how to fix it**, with the exact command when
  there is one. The command line guides (`keel doctor`); no setup scripts.
- Format with `rustfmt.toml` (120 columns).

## Working with the user

- Work on a branch and open a pull request; the user reviews the diff there
  and comments. Its title becomes the release notes' line: write it for
  someone upgrading. Label it `breaking`, `feature` or `fix`.
- Commit and push only when asked. Never force-push or rewrite `main`.
- Ask before anything that's hard to undo or visible to others (releases,
  deleting branches, changing repository settings).
