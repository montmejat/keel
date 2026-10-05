# The control protocol

What `keel top`, `keel logs`, `keel-mcp` and every other tool use to look at a
running daemon or coordinator, and to act on it. The types are in
`crates/keel-daemon/src/control.rs`; this is the same thing in words.
Version **1**.

## Transport

- A Unix socket per daemon: `$XDG_RUNTIME_DIR/keel/<pid>/control.sock`. A
  daemon is running if that directory's pid is alive.
- **Newline-delimited JSON**: one request per line, one reply per line, in
  order, several per connection. You can drive it by hand:
  `echo '{"cmd":"status"}' | socat - UNIX-CONNECT:$XDG_RUNTIME_DIR/keel/<pid>/control.sock`
- Access is the file permissions of the runtime directory: per user.
- A coordinator serves the same protocol as a daemon, and answers for every
  machine of its dataflow. That is the one to ask in a multi-machine
  dataflow (`Status.coordinator` says which it is).

## Requests

A request is an object with a `cmd`. A reply is an object with one key, the
reply's name. Any request may be answered with `{"error": "<text>"}`.

| `cmd` | Other fields | Reply | What it does |
|---|---|---|---|
| `hello` | | `hello` | `{protocol, keel}`: the protocol version and the keel build |
| `status` | | `status` | The graph: nodes, links, uptime, deployment |
| `logs` | `since` | `logs` | Lines numbered `since` and after, from the last 10 000 kept |
| `trace` | `summary` (default false) | `trace` | Latency per input; unless `summary`, also the sampled traces |
| `subscribe` | `interval_ms` (default 250, at least 50) | `subscribed`, then `event`s | Turns the connection into a stream, see below |
| `stop` | | `stopping` | Stops the dataflow gracefully |
| `update` | `deployment`, `binaries` | `updating` | Rolls a new deployment in: nodes whose binary changed restart, one at a time. Answers with the nodes it replaces |

`status`, `logs`, `trace` and `subscribe` only read. `stop` and `update` act.

## Streaming

`{"cmd":"subscribe","interval_ms":250}` answers `{"subscribed":{"interval_ms":250}}`,
then, every interval until the client hangs up, up to three events:

```json
{"event":{"status":{...}}}     the same as a `status` reply
{"event":{"logs":{...}}}       the lines since the last event; not sent when there are none
{"event":{"latency":{...}}}    the same as `trace` with `summary`
```

The connection takes no more requests once subscribed: open another for
those. The daemon samples at the interval and sends what it finds, so an
event is at most one interval late; it doesn't yet send on change. Counters
move all the time, so a status would nearly always have changed anyway.
`keel events` prints a stream as one JSON object per line.

## Starting a conversation

A client asks `hello` first. If `protocol` is not the one it was built for,
it stops with an error naming both versions: the daemon may be from a keel
deployed before the tool, or after it. A daemon that answers `hello` with an
error is from before the protocol had a version, and counts as version 0.
`keel_daemon::control::Client::connect` does this; tools built on it get the
check for nothing.

The version goes up when a request or reply changes in a way an older client
would misread. A field added with a default doesn't change it.

## Picking a daemon

`control::running()` lists the daemons with their status, and
`control::pick(pid)` chooses one: the pid given, else the only daemon
running, else the only coordinator. Several and no way to choose is an
error that lists them. Every tool uses these two.

## Reading the answers

- **Times** in `status` and `logs` are milliseconds since the daemon started;
  in `trace`, nanoseconds on one clock (the machine's, or the coordinator's
  once merged).
- **`logs`** is polled: pass the `next` of the last reply as `since`.
- **`trace`** says where the time went per input: `latency` is published →
  taken by the receiver, `processing` is taken → released.
  `clock_error_ns` is how far off a cross-machine latency may be.
- **Polling** costs a round trip per tool per refresh: negligible at the 4 Hz
  of `keel top`.

## Not here yet

Written down so the protocol grows in one direction. None of it exists.

- **Streams driven by change.** Events sent when something happens (a node
  exits, a line is logged), not sampled.
- **Rollback and start as requests.** `keel rollback` and `keel start` act
  on the deployment store of the machine you type them on, not through the
  control API.
- **Asking before acting.** An acting request from an agent waits for a human
  to say yes: a `pending` reply, an `approve` request, a record of who did what.
- **Over the network.** The socket is local. A gateway for a browser would
  speak the same messages over HTTP and WebSocket.
