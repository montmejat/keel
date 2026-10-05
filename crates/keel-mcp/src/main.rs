//! An MCP server for an agent to look at a running dataflow, and to ask for
//! changes to it: the same control API `keel top` and `keel logs` use.
//!
//! It connects as an agent, so what it asks that changes something (`stop`,
//! `restart_node`) doesn't happen until a person says yes, with `keel approve`.
//! The tool waits for the answer, and tells the agent what it was.
//!
//! MCP is JSON-RPC 2.0, one message per line, on stdin and stdout. Only what
//! a client needs to list and call tools is implemented.
//!
//! ```sh
//! claude mcp add keel -- ./target/release/keel-mcp
//! ```

use std::io::{self, BufRead, Write};
use std::time::{Duration, Instant};

use keel_daemon::control::{self, ActionState, Client, NodeState, Reply};
use keel_daemon::doctor;
use serde_json::{json, Value};

/// How long an acting tool waits for a person before telling the agent that it
/// is still waiting, and to ask again with `check_action`. Short enough to
/// stay inside an MCP client's patience for one call.
const WAIT_FOR_APPROVAL: Duration = Duration::from_secs(25);

/// A reply longer than this is cut: an agent's context is not free.
const MAX_LOG_LINES: usize = 200;

fn main() -> io::Result<()> {
    let mut calls: Vec<std::thread::JoinHandle<()>> = Vec::new();
    for line in io::stdin().lock().lines() {
        calls.retain(|c| !c.is_finished());
        let Ok(message) = serde_json::from_str::<Value>(&line?) else { continue };
        // A notification has no id and gets no answer.
        let Some(id) = message.get("id").cloned() else { continue };
        // Each on its own thread: a tool waiting for a person mustn't keep
        // the others, or a ping, from being answered. Replies may come out of
        // order, which JSON-RPC allows: the id says which is which.
        calls.push(std::thread::spawn(move || {
            let reply = match respond(message["method"].as_str().unwrap_or(""), &message["params"]) {
                Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
                Err(e) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": e}}),
            };
            let mut out = io::stdout().lock();
            let _ = writeln!(out, "{reply}").and_then(|_| out.flush());
        }));
    }
    // The client is done asking; answer what it asked before going.
    calls.into_iter().for_each(|c| drop(c.join()));
    Ok(())
}

fn respond(method: &str, params: &Value) -> Result<Value, String> {
    match method {
        "initialize" => Ok(json!({
            "protocolVersion": params["protocolVersion"].as_str().unwrap_or("2024-11-05"),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "keel", "version": env!("CARGO_PKG_VERSION")},
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tools()})),
        "tools/call" => {
            let args = &params["arguments"];
            let (text, is_error) = match call(params["name"].as_str().unwrap_or(""), args) {
                Ok(text) => (text, false),
                Err(e) => (e, true),
            };
            Ok(json!({"content": [{"type": "text", "text": text}], "isError": is_error}))
        }
        other => Err(format!("method not found: {other}")),
    }
}

fn tools() -> Value {
    let pid = json!({"type": "integer", "description": "The daemon's pid, from `dataflows`. Optional when only one is running."});
    json!([
        {
            "name": "dataflows",
            "description": "List the running keel dataflows: pid, uptime, how many nodes are running, and the dataflow file.",
            "inputSchema": {"type": "object", "properties": {}},
        },
        {
            "name": "status",
            "description": "The graph of a running dataflow: every node with its state, restart count and, if it exited, why; every link with the messages and bytes that went through it. A node that restarts often, or a link that has stopped moving, is where to look.",
            "inputSchema": {"type": "object", "properties": {"pid": pid}},
        },
        {
            "name": "logs",
            "description": "The last lines the nodes printed, oldest first, with the time since the daemon started.",
            "inputSchema": {"type": "object", "properties": {
                "pid": pid,
                "node": {"type": "string", "description": "Only this node's lines."},
                "last": {"type": "integer", "description": "How many lines, at most 200. Default 50."},
            }},
        },
        {
            "name": "latency",
            "description": "For each input of each node: how long its messages took to arrive (`latency`) and to be processed (`processing`), as p50, p99, p99.9 and max in microseconds. A large processing time is a slow node; a large latency with a small processing time is a node that wasn't listening.",
            "inputSchema": {"type": "object", "properties": {"pid": pid}},
        },
        {
            "name": "restart_node",
            "description": "Kill a node so that it starts again, as its restart policy would after a crash. For a node that is stuck or stalled and has `restart` set in the dataflow. This changes the running system, so a person has to approve it first: the call waits for them (up to 25 s) and says what they decided, or gives the action's id to check again with `check_action`.",
            "inputSchema": {"type": "object", "required": ["node"], "properties": {
                "pid": pid,
                "node": {"type": "string", "description": "The node's id, from `status`."},
            }},
        },
        {
            "name": "stop",
            "description": "Stop the whole dataflow gracefully. A person has to approve it first: the call waits for them (up to 25 s) and says what they decided, or gives the action's id to check again with `check_action`. Use it only when nothing smaller fixes the problem.",
            "inputSchema": {"type": "object", "properties": {"pid": pid}},
        },
        {
            "name": "check_action",
            "description": "What became of something you asked for that was still waiting for a person: waits up to 25 s more for their decision.",
            "inputSchema": {"type": "object", "required": ["id"], "properties": {
                "pid": pid,
                "id": {"type": "integer", "description": "The action's id, as `restart_node` or `stop` gave it."},
            }},
        },
        {
            "name": "doctor",
            "description": "Is this machine fit to run a robot: real-time limits, memory locking, CPU governor, clock, swap. Each check is ok, warn or fail, with what it costs.",
            "inputSchema": {"type": "object", "properties": {}},
        },
    ])
}

fn call(tool: &str, args: &Value) -> Result<String, String> {
    let json = |v: Value| serde_json::to_string_pretty(&v).map_err(|e| e.to_string());
    match tool {
        "dataflows" => json(serde_json::to_value(control::summaries()).map_err(|e| e.to_string())?),
        "status" => {
            let status = client(args)?.status().map_err(|e| e.to_string())?;
            let nodes: Vec<Value> = (status.nodes.iter())
                .map(|n| {
                    let state = match &n.state {
                        NodeState::Exited { success, detail } => {
                            format!("exited ({}): {detail}", if *success { "ok" } else { "failed" })
                        }
                        other => format!("{other:?}").to_lowercase(),
                    };
                    json!({"id": n.id, "state": state, "restarts": n.restarts, "program": n.program,
                           "machine": n.machine, "shm_held": n.shm_held})
                })
                .collect();
            let links: Vec<Value> = (status.links.iter())
                .map(|l| json!({"source": l.source, "targets": l.targets, "messages": l.messages, "bytes": l.bytes}))
                .collect();
            json(json!({"dataflow": status.dataflow, "uptime_ms": status.uptime_ms, "stopping": status.stopping,
                        "deployment": status.deployment, "nodes": nodes, "links": links}))
        }
        "logs" => {
            let last = (args["last"].as_u64().unwrap_or(50) as usize).min(MAX_LOG_LINES);
            let logs = client(args)?.logs(0).map_err(|e| e.to_string())?;
            let mut lines: Vec<String> = (logs.lines.iter())
                .filter(|l| args["node"].as_str().is_none_or(|n| n == l.node))
                .map(|l| format!("{:>8.3}s {:<12} {}", l.t_ms as f64 / 1e3, l.node, l.text))
                .collect();
            lines.drain(..lines.len().saturating_sub(last));
            Ok(if lines.is_empty() { "(no lines)".into() } else { lines.join("\n") })
        }
        "latency" => {
            let report = client(args)?.latency().map_err(|e| e.to_string())?;
            let us = |ns: u64| (ns as f64 / 100.0).round() / 10.0;
            let rows: Vec<Value> = (report.inputs.iter())
                .map(|i| {
                    let p = |p: &keel_daemon::control::Percentiles| {
                        json!({"count": p.count, "p50_us": us(p.p50), "p99_us": us(p.p99), "p999_us": us(p.p999), "max_us": us(p.max)})
                    };
                    json!({"input": format!("{}/{}", i.node, i.input), "source": i.source,
                           "latency": p(&i.latency), "processing": p(&i.processing)})
                })
                .collect();
            json(json!(rows))
        }
        "restart_node" => {
            let node = args["node"].as_str().ok_or("`node` is required")?;
            let mut client = agent_client(args)?;
            ask(&mut client, |c| c.restart(node))
        }
        "stop" => {
            let mut client = agent_client(args)?;
            ask(&mut client, |c| c.request(&control::Request::Stop))
        }
        "check_action" => {
            let id = args["id"].as_u64().ok_or("`id` is required")?;
            wait_for(&mut agent_client(args)?, id)
        }
        "doctor" => {
            let checks: Vec<Value> = (doctor::here().iter())
                .map(|c| json!({"check": c.name, "level": format!("{:?}", c.level).to_lowercase(), "detail": c.detail}))
                .collect();
            json(json!(checks))
        }
        other => Err(format!("no such tool: {other}")),
    }
}

/// A connection that holds what it asks for until a person approves it.
fn agent_client(args: &Value) -> Result<Client, String> {
    let pid = control::pick(args["pid"].as_u64().map(|p| p as u32)).map_err(|e| e.replace("--pid", "`pid`"))?;
    Client::connect_as_agent(pid, "keel-mcp").map_err(|e| e.to_string())
}

/// Sends a request that changes something, and waits for a person's answer.
fn ask(client: &mut Client, send: impl FnOnce(&mut Client) -> io::Result<Reply>) -> Result<String, String> {
    match send(client).map_err(|e| e.to_string())? {
        Reply::Pending { id } => wait_for(client, id),
        other => Ok(format!("done: {}", control::outcome(&other))),
    }
}

/// Waits for a person to decide on action `id`, for a while.
fn wait_for(client: &mut Client, id: u64) -> Result<String, String> {
    let until = Instant::now() + WAIT_FOR_APPROVAL;
    while Instant::now() < until {
        let actions = client.actions().map_err(|e| e.to_string())?;
        let Some(action) = actions.iter().find(|a| a.id == id) else {
            return Err(format!("There is no action {id} on this dataflow."));
        };
        // Approved is said before the action runs, its outcome after: wait for both.
        match (action.state, &action.outcome) {
            (ActionState::Denied, _) => {
                return Err(format!("A person denied it (action {id}). Don't try it again; say what you found instead."))
            }
            (ActionState::Approved, Some(outcome)) => return Ok(format!("A person approved it, and: {outcome}")),
            _ => {}
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Ok(format!("Still waiting for a person (action {id}). They can approve it with `keel approve {id}` or in `keel web`. Call `check_action` with id {id} to keep waiting, or tell them what you want and why."))
}

/// The daemon to ask: the `pid` argument, else the only one running.
fn client(args: &Value) -> Result<Client, String> {
    let pid = control::pick(args["pid"].as_u64().map(|p| p as u32)).map_err(|e| e.replace("--pid", "`pid`"))?;
    Client::connect(pid).map_err(|e| e.to_string())
}
