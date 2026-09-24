# 0011: Control API: JSON lines over a Unix socket

**Status:** Proposed, 2026-09-24

## Context
Tools (`keel ps`, `logs`, `stop`, `top`) and, later, the coordinator need to
inspect and drive a running daemon ([0007](0007-tools-are-control-plane-clients.md)).

## Decision
- Each daemon serves `$XDG_RUNTIME_DIR/keel/<pid>/control.sock`.
- **Discovery** is a directory listing: every `<pid>` directory whose process
  is alive is a running daemon. There's no registry to go stale beyond what
  the startup cleanup removes.
- **Protocol:** newline-delimited JSON, one request and one reply per line,
  several per connection. Requests: `status`, `logs {since}`, `stop`. You can
  drive it by hand:
  `echo '{"cmd":"status"}' | socat - UNIX-CONNECT:$XDG_RUNTIME_DIR/keel/<pid>/control.sock`
- **Polling, not streaming.** Clients poll `status` and `logs {since}`. The
  daemon keeps only totals and a ring buffer of the last 10 000 log lines;
  clients compute rates from deltas.
- The types live in `keel_daemon::control`, shared by server and client.

## Consequences
- Unlike the node protocol ([0008](0008-hand-rolled-wire-format.md)), this
  one favours readability over speed. That's fine, since it isn't on the hot path.
- Polling costs one round trip per tool per refresh: negligible at 4 Hz.
- Access control is file permissions on the runtime dir, which is per-user.
- Only works on the daemon's machine. Multi-machine (milestone 4) needs the
  same API over TCP, or the coordinator relaying it.
