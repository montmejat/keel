//! An MCP server for an agent to look at a running dataflow: the same control
//! API `keel top` and `keel logs` use, and nothing more. Every tool only
//! reads; there is nothing here that stops, updates or changes anything.
//!
//! MCP is JSON-RPC 2.0, one message per line, on stdin and stdout. Only what
//! a client needs to list and call tools is implemented.
//!
//! ```sh
//! claude mcp add keel -- ./target/release/keel-mcp
//! ```

use std::io::{self, BufRead, Write};

use keel_daemon::control::{self, Client, NodeState};
use keel_daemon::doctor;
use serde_json::{json, Value};

/// A reply longer than this is cut: an agent's context is not free.
const MAX_LOG_LINES: usize = 200;

fn main() -> io::Result<()> {
    let stdout = io::stdout();
    for line in io::stdin().lock().lines() {
        let Ok(message) = serde_json::from_str::<Value>(&line?) else { continue };
        // A notification has no id and gets no answer.
        let Some(id) = message.get("id").cloned() else { continue };
        let reply = match respond(message["method"].as_str().unwrap_or(""), &message["params"]) {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(e) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": e}}),
        };
        let mut out = stdout.lock();
        writeln!(out, "{reply}")?;
        out.flush()?;
    }
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
            "name": "doctor",
            "description": "Is this machine fit to run a robot: real-time limits, memory locking, CPU governor, clock, swap. Each check is ok, warn or fail, with what it costs.",
            "inputSchema": {"type": "object", "properties": {}},
        },
    ])
}

fn call(tool: &str, args: &Value) -> Result<String, String> {
    let json = |v: Value| serde_json::to_string_pretty(&v).map_err(|e| e.to_string());
    match tool {
        "dataflows" => json(dataflows()),
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
        "doctor" => {
            let checks: Vec<Value> = (doctor::here().iter())
                .map(|c| json!({"check": c.name, "level": format!("{:?}", c.level).to_lowercase(), "detail": c.detail}))
                .collect();
            json(json!(checks))
        }
        other => Err(format!("no such tool: {other}")),
    }
}

fn dataflows() -> Value {
    let rows: Vec<Value> = (control::running().into_iter())
        .map(|(pid, status)| {
            let running = status.nodes.iter().filter(|n| n.state == NodeState::Running).count();
            json!({"pid": pid, "uptime_ms": status.uptime_ms, "nodes_running": running,
                   "nodes": status.nodes.len(), "machine": status.machine,
                   "coordinator": status.coordinator, "dataflow": status.dataflow})
        })
        .collect();
    json!(rows)
}

/// The daemon to ask: the `pid` argument, else the only one running.
fn client(args: &Value) -> Result<Client, String> {
    let pid = control::pick(args["pid"].as_u64().map(|p| p as u32)).map_err(|e| e.replace("--pid", "`pid`"))?;
    Client::connect(pid).map_err(|e| e.to_string())
}
