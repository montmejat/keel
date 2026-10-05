# 0030: An agent looks through the control API, and only looks

**Status:** Proposed, 2026-10-05

## Context
An agent that can inspect a robot's software graph and say what is wrong
with it is the next layer up. keel already has everything such a tool needs:
`keel top`, `ps`, `logs` and `trace` are clients of the control API
([0007](0007-tools-are-control-plane-clients.md),
[0011](0011-control-api.md)). The question is how an agent gets at it, and
what it may do there.

## Decision
- **An MCP server, `keel-mcp`,** over the control API: JSON-RPC on stdin and
  stdout, written by hand on `serde_json` like the rest of keel's protocols
  ([0008](0008-hand-rolled-wire-format.md)). Any agent that speaks MCP can use it;
  no new dependency.
- **Read-only to begin with.** Five tools: `dataflows`, `status`, `logs`,
  `latency`, `doctor`. Nothing stops, updates or changes a thing. Acting
  (`stop`, `update`, `rollback`) comes once there is a way to ask before it
  happens.
- **Answers made for reading.** Nodes' states in words, times in
  microseconds, logs cut to the last 200 lines; each tool's description says
  what a number means (a large processing time is a slow node, a large
  latency with a small processing time is one that wasn't listening),
  because the agent has only the descriptions to go on.
- **Scenarios to measure it.** `examples/agent-{slow,stall,crash}.yml`: a
  filter between a sensor and a sink that falls behind, stops without dying,
  or crashes in a loop. `examples/agent/run.sh` starts one, asks `claude -p`
  what is wrong, and checks that the answer names the filter and its kind of
  fault. The agent runs on whatever account `claude` is logged in to.

## Consequences
- The first run of each scenario, with Sonnet, passed. That is three runs of
  one model on three easy faults: it shows the tools carry enough to
  diagnose by, not how good an agent is. The check is a keyword match on the
  answer, and lenient.
- The stall is the instructive one. No log line, no crash, state `running`:
  the agent found it from a link's message count standing still while its
  input's kept rising. That evidence was in `status` already; it took no
  new instrumentation.
- Not here yet: acting on what it found, asking before it does, faults a
  recording would explain, and code the agent writes itself to look
  further.
