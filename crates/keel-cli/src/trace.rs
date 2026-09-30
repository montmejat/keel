//! `keel trace`: where the time goes.
//!
//! Prints the latency and processing time of every input, then the latest
//! sampled traces as trees: each message, the hops it took to each receiver,
//! and the messages that receiver sent while handling it. `--export` writes
//! the sampled traces as Chrome trace JSON, to open in ui.perfetto.dev.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::Path;

use keel::trace::span_seq;
use keel_daemon::control::{Client, InputReport, SpanRecord, TraceReport};
use serde_json::{json, Value};

use crate::fmt::nanos;

pub fn run(pid: u32, traces: usize, export: Option<&Path>) -> io::Result<()> {
    let report = Client::connect(pid)?.trace()?;
    let multi = report.inputs.iter().any(|i| i.machine.is_some());
    print_inputs(&report.inputs, multi);
    if !report.clocks.is_empty() {
        let clocks: Vec<String> = (report.clocks.iter())
            .map(|(m, c)| format!("{m} {:+.3}ms ±{}", c.offset_ns as f64 / 1e6, nanos(c.error_ns)))
            .collect();
        println!("\nclocks (minus the coordinator's): {}", clocks.join(", "));
    }
    if report.dropped_events > 0 {
        println!("\n{} trace events were dropped: a node's ring was full", report.dropped_events);
    }
    if traces > 0 {
        print_traces(&report, traces, multi);
    }
    if let Some(path) = export {
        std::fs::write(path, serde_json::to_vec(&chrome_trace(&report))?)?;
        println!("\nwrote {} spans to {} (open it in ui.perfetto.dev)", report.spans.len(), path.display());
    }
    Ok(())
}

fn at(name: &str, machine: &Option<String>, multi: bool) -> String {
    match machine {
        Some(m) if multi => format!("{name}@{m}"),
        _ => name.to_owned(),
    }
}

fn print_inputs(inputs: &[InputReport], multi: bool) {
    println!(
        "{:<28} {:<28} {:>7}  {:>9} {:>9} {:>9} {:>9}  {:>9} {:>9} {:>9}",
        "INPUT", "FROM", "MSGS", "LAT p50", "p99", "p99.9", "max", "PROC p50", "p99", "max"
    );
    for i in inputs {
        let clock = match i.clock_error_ns {
            Some(0) => String::new(),
            Some(e) => format!("  ±{}", nanos(e)),
            None => "  clocks not aligned yet".into(),
        };
        println!(
            "{:<28} {:<28} {:>7}  {:>9} {:>9} {:>9} {:>9}  {:>9} {:>9} {:>9}{clock}",
            at(&format!("{}/{}", i.node, i.input), &i.machine, multi),
            at(&i.source, &i.source_machine, multi),
            i.latency.count,
            nanos(i.latency.p50),
            nanos(i.latency.p99),
            nanos(i.latency.p999),
            nanos(i.latency.max),
            nanos(i.processing.p50),
            nanos(i.processing.p99),
            nanos(i.processing.max),
        );
    }
}

/// The latest `count` traces whose first message we saw, oldest first.
fn print_traces(report: &TraceReport, count: usize, multi: bool) {
    let mut children: HashMap<u64, Vec<&SpanRecord>> = HashMap::new();
    for span in &report.spans {
        children.entry(span.parent).or_default().push(span);
    }
    // Finished traces first: the latest ones are usually still in flight.
    let mut roots: Vec<&SpanRecord> =
        (report.spans.iter()).filter(|s| s.parent == 0 && s.published.is_some()).collect();
    if roots.iter().any(|s| last_release(s, &children).is_some()) {
        roots.retain(|s| last_release(s, &children).is_some());
    }
    roots.sort_by_key(|s| s.published);
    let roots = &roots[roots.len().saturating_sub(count)..];
    if roots.is_empty() {
        println!("\nno sampled traces yet");
        return;
    }
    for root in roots {
        let start = root.published.unwrap();
        let end = last_release(root, &children).unwrap_or(start);
        println!(
            "\n{} #{}: {} end to end",
            at(&root.source, &root.source_machine, multi),
            span_seq(root.span),
            nanos(end - start)
        );
        print_span(root, &children, 1, multi);
    }
}

fn last_release(span: &SpanRecord, children: &HashMap<u64, Vec<&SpanRecord>>) -> Option<u64> {
    let own = span.deliveries.iter().filter_map(|d| d.released).max();
    let below = children.get(&span.span).into_iter().flatten().filter_map(|c| last_release(c, children)).max();
    own.max(below)
}

fn print_span(span: &SpanRecord, children: &HashMap<u64, Vec<&SpanRecord>>, depth: usize, multi: bool) {
    let indent = "  ".repeat(depth);
    if span.deliveries.is_empty() {
        println!("{indent}→ (in flight: no receiver has it yet)");
    }
    for d in &span.deliveries {
        let latency = span.published.zip(d.taken).map_or("?".into(), |(p, t)| nanos(t.saturating_sub(p)));
        let processing = d.taken.zip(d.released).map_or("?".into(), |(t, r)| nanos(r.saturating_sub(t)));
        println!(
            "{indent}→ {}  {}  {}  then processing {processing}",
            at(&format!("{}/{}", d.node, d.input), &d.machine, multi),
            latency,
            hops(span, d),
        );
        let caused = children.get(&span.span).into_iter().flatten();
        for child in caused.filter(|c| c.source.split('/').next() == Some(d.node.as_str())) {
            println!("{indent}  {} #{}", child.source, span_seq(child.span));
            print_span(child, children, depth + 2, multi);
        }
    }
}

/// `[send 3.1µs, net 470ms, deliver 12µs, wake 8.0µs]`: each step to one
/// receiver, where known.
fn hops(span: &SpanRecord, d: &keel_daemon::control::Delivery) -> String {
    let received = d.machine.as_ref().and_then(|m| span.net_received.get(m)).copied();
    let sent = d.machine.as_ref().and_then(|m| span.net_sent.get(m)).copied();
    let steps = [
        ("send", span.published, span.routed),
        ("daemon", sent.and(span.routed), sent),
        ("network", sent, received),
        ("route", received.or(span.routed), d.delivered),
        ("wake", d.delivered, d.taken),
    ];
    let steps: Vec<String> = (steps.iter())
        .filter_map(|&(name, from, to)| Some(format!("{name} {}", nanos(to?.saturating_sub(from?)))))
        .collect();
    format!("[{}]", steps.join(", "))
}

/// Chrome trace JSON: a process per machine, a track per node with its
/// processing slices, a track per node inbox with each message's transit,
/// and arrows from each publish to each take.
fn chrome_trace(report: &TraceReport) -> Value {
    let mut pids: BTreeMap<String, u64> = BTreeMap::new();
    let mut tids: BTreeMap<String, u64> = BTreeMap::new();
    let mut events = Vec::new();
    let id = |map: &mut BTreeMap<String, u64>, key: &str| {
        let next = map.len() as u64 + 1;
        *map.entry(key.to_owned()).or_insert(next)
    };
    let us = |ns: u64| ns as f64 / 1e3;
    for span in &report.spans {
        let (Some(published), Some(source_node)) = (span.published, span.source.split('/').next()) else { continue };
        let source_machine = span.source_machine.clone().unwrap_or_else(|| "local".into());
        let (spid, stid) = (id(&mut pids, &source_machine), id(&mut tids, source_node));
        for (n, d) in span.deliveries.iter().enumerate() {
            let (Some(taken), Some(released)) = (d.taken, d.released) else { continue };
            let machine = d.machine.clone().unwrap_or_else(|| "local".into());
            let (pid, tid) = (id(&mut pids, &machine), id(&mut tids, &d.node));
            let args = json!({ "trace": span.trace, "span": span.span, "seq": span_seq(span.span) });
            events.push(json!({ "name": format!("{} ← {}", d.input, span.source), "cat": "transit", "ph": "X",
                "ts": us(published), "dur": us(taken.saturating_sub(published)), "pid": pid, "tid": tid + 10_000, "args": args }));
            events.push(json!({ "name": d.input, "cat": "processing", "ph": "X",
                "ts": us(taken), "dur": us(released.saturating_sub(taken)), "pid": pid, "tid": tid, "args": args }));
            let flow = span.span.wrapping_mul(16).wrapping_add(n as u64);
            events.push(json!({ "name": span.source, "cat": "flow", "ph": "s", "id": flow, "ts": us(published), "pid": spid, "tid": stid }));
            events.push(json!({ "name": span.source, "cat": "flow", "ph": "f", "bp": "e", "id": flow, "ts": us(taken), "pid": pid, "tid": tid }));
        }
    }
    for (machine, pid) in &pids {
        events.push(json!({ "name": "process_name", "ph": "M", "pid": pid, "args": { "name": machine } }));
        for (node, tid) in &tids {
            events.push(json!({ "name": "thread_name", "ph": "M", "pid": pid, "tid": tid, "args": { "name": node } }));
            events.push(json!({ "name": "thread_name", "ph": "M", "pid": pid, "tid": tid + 10_000, "args": { "name": format!("{node} inbox") } }));
        }
    }
    json!({ "traceEvents": events, "displayTimeUnit": "ns" })
}
