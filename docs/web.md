# keel web

A page in the browser over the control API: the graph with the rate on each
link, latency and restarts per node, the logs, and what an agent asked to do,
with buttons to approve or deny it. It is `crates/keel-web`: an HTTP server
on `std::net`, and one HTML file embedded in the binary. No framework, no
bundler, no dependency.

```sh
keel run examples/pipeline.yml      # or any dataflow
keel web                            # prints http://127.0.0.1:7600/#<token>
```

Open the address it prints, token included. It finds the running daemon
itself, and finds the next one if the dataflow is restarted: the page says
"waiting for a dataflow" until there is one.

## What it shows

- **The graph**, nodes in columns from the sources, links with their message
  rate over the last second. A link whose source is running but whose
  rate is zero is dashed red and says *stalled*; a node that takes messages
  in and sends none out says *not sending*, whatever its state is. That is
  how the stalled filter of `examples/agent/run.sh stall` looks.
- **Per node:** its state, restarts, and the worst p99 processing time of
  its inputs. Click a node to filter the logs to it, and to restart it.
- **Needs you:** what an agent asked to do ([protocol](protocol.md), *Agents
  ask, people decide*), with Approve and Deny, and the last decisions with
  what came of them.
- **Logs**, newest at the bottom.

## How it talks

| | |
|---|---|
| `GET /` | the page |
| `GET /api/events?token=` | server-sent events: `status`, `logs`, `latency`, as the control API's `subscribe` sends them; `waiting` when there is no dataflow |
| `GET /api/dataflows`, `/api/actions` | JSON |
| `POST /api/approve?id=`, `/api/deny?id=`, `/api/restart?node=`, `/api/stop` | act, as a person |

Server-sent events rather than WebSocket: plain HTTP, no handshake to write
by hand, and the browser reconnects by itself.

## Why the token

The page is what stands between an agent and the machine, since approving is
the guard on what an agent asks. It must not be possible for another website
to approve for you. So:

- it listens on `127.0.0.1` (it warns when told otherwise);
- it refuses a `Host` that isn't its own address, against DNS rebinding;
- every `/api` request carries the random token printed at start. Reads take
  it in the query, because an `EventSource` can't set headers. A `POST` takes
  it **only as the `X-Keel-Token` header**, which a page on another origin
  can't send without a CORS preflight, and the server doesn't answer one.

The token lives in the URL's fragment, which the browser doesn't send to the
server; the page reads it from there.

Not here: other users on the machine can reach the port, and the token is
the only thing between them and the control socket's permissions. TLS and
accounts are for the day it listens elsewhere.
