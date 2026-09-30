//! The daemon's side of tracing (see `keel::trace`).
//!
//! Nodes count every message in their stats files and write hop events for
//! sampled traces to their event rings. The daemon drains the rings every
//! few milliseconds and adds the hops only it sees (routing, the network,
//! delivery) to build a [`SpanRecord`] per sampled message, keeping the
//! latest [`MAX_SPANS`].
//!
//! Timestamps from another machine are converted to this machine's clock
//! with the offsets the coordinator measures, so latency across machines is
//! meaningful, within the error it reports.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Mutex;

use keel::trace::{self, Context, EventReader, RawEvent, StatsReader};

use crate::control::{Clock, Delivery, InputReport, SpanRecord, TraceReport};

/// Sampled messages kept per daemon.
const MAX_SPANS: usize = 4096;

pub(crate) struct Tracing {
    shm_dir: PathBuf,
    machine: Option<String>,
    machine_of: HashMap<String, String>,
    /// `(node, input)` -> `(source node, output)`, for local nodes.
    feeds: BTreeMap<(String, String), (String, String)>,
    inner: Mutex<Inner>,
}

struct Inner {
    files: BTreeMap<String, NodeFiles>,
    spans: HashMap<u64, SpanRecord>,
    order: VecDeque<u64>,
    clocks: BTreeMap<String, Clock>,
    events: Vec<RawEvent>,
}

/// A node's tracing files, opened once the node has created them.
#[derive(Default)]
struct NodeFiles {
    stats: Option<StatsReader>,
    events: Option<EventReader>,
}

impl Tracing {
    pub fn new(
        shm_dir: PathBuf,
        machine: Option<String>,
        machine_of: HashMap<String, String>,
        feeds: BTreeMap<(String, String), (String, String)>,
        local_nodes: impl IntoIterator<Item = String>,
    ) -> Self {
        let files = local_nodes.into_iter().map(|id| (id, NodeFiles::default())).collect();
        let inner =
            Inner { files, spans: HashMap::new(), order: VecDeque::new(), clocks: BTreeMap::new(), events: Vec::new() };
        Self { shm_dir, machine, machine_of, feeds, inner: Mutex::new(inner) }
    }

    /// The local daemon read `source/output`'s descriptor.
    pub fn routed(&self, context: &Context, source: &str, output: &str, t: u64) {
        let source_machine = self.machine_of.get(source).cloned();
        self.inner.lock().unwrap().with_span(context, |span| {
            span.source = format!("{source}/{output}");
            span.source_machine = source_machine;
            span.routed = Some(t);
        });
    }

    pub fn delivered(&self, context: &Context, node: &str, input: &str, t: u64) {
        let machine = self.machine.clone();
        self.inner.lock().unwrap().with_span(context, |span| span.delivery(node, input, machine).delivered = Some(t));
    }

    pub fn net_sent(&self, context: &Context, to_machine: &str, t: u64) {
        self.inner.lock().unwrap().with_span(context, |span| {
            span.net_sent.insert(to_machine.to_owned(), t);
        });
    }

    /// A message from another machine, whose context has already been
    /// converted to this machine's clock.
    pub fn net_received(&self, context: &Context, source: &str, output: &str, t: u64) {
        let (source_machine, machine) = (self.machine_of.get(source).cloned(), self.machine.clone());
        self.inner.lock().unwrap().with_span(context, |span| {
            span.source = format!("{source}/{output}");
            span.source_machine = source_machine;
            span.published = Some(context.published_ns);
            if let Some(machine) = machine {
                span.net_received.insert(machine, t);
            }
        });
    }

    pub fn set_clocks(&self, clocks: BTreeMap<String, Clock>) {
        self.inner.lock().unwrap().clocks = clocks;
    }

    /// Converts a time on `source`'s machine to this machine's clock. Left as
    /// is while either clock is unknown.
    pub fn to_local(&self, t: u64, source: &str) -> u64 {
        let inner = self.inner.lock().unwrap();
        let (Some(from), Some(here)) = (self.machine_of.get(source), self.machine.as_ref()) else { return t };
        match (inner.clocks.get(from), inner.clocks.get(here)) {
            (Some(from), Some(here)) => (t as i128 - from.offset_ns as i128 + here.offset_ns as i128).max(0) as u64,
            _ => t,
        }
    }

    /// Opens the files of nodes that have created theirs since last time.
    fn open_files(&self, inner: &mut Inner) {
        for (node, files) in inner.files.iter_mut() {
            if files.stats.is_none() {
                files.stats = StatsReader::open(&self.shm_dir, node).ok();
            }
            if files.events.is_none() {
                files.events = EventReader::open(&self.shm_dir, node).ok();
            }
        }
    }

    /// `(node, output, messages, bytes)` sent by each local node.
    pub fn outputs(&self) -> Vec<(String, String, u64, u64)> {
        let mut inner = self.inner.lock().unwrap();
        self.open_files(&mut inner);
        let stats = inner.files.iter().filter_map(|(node, files)| Some((node, files.stats.as_ref()?)));
        stats.flat_map(|(node, stats)| stats.outputs().into_iter().map(|(o, n, b)| (node.clone(), o, n, b))).collect()
    }

    /// Messages sent by local nodes so far: moves as long as the dataflow does.
    pub fn messages_sent(&self) -> u64 {
        self.outputs().iter().map(|(_, _, n, _)| n).sum()
    }

    /// Drains the nodes' event rings into span records.
    pub fn collect(&self) {
        let mut guard = self.inner.lock().unwrap();
        self.open_files(&mut guard);
        let inner = &mut *guard;
        let machine = self.machine.clone();
        for (node, files) in inner.files.iter_mut() {
            let Some(events) = &files.events else { continue };
            inner.events.clear();
            events.drain(&mut inner.events);
            for event in &inner.events {
                let context = Context {
                    span: event.span,
                    trace: event.trace,
                    parent: event.parent,
                    published_ns: 0,
                    sampled: true,
                };
                let input = || files.stats.as_ref().and_then(|s| s.input_name(event.aux as usize)).unwrap_or_default();
                let record = |span: &mut SpanRecord| match event.kind {
                    trace::PUBLISHED => {
                        span.published = Some(event.t_ns);
                        let output = files.stats.as_ref().and_then(|s| s.output_name(event.aux as usize));
                        span.source = format!("{node}/{}", output.as_deref().unwrap_or("?"));
                        span.source_machine = machine.clone();
                    }
                    trace::TAKEN => span.delivery(node, &input(), machine.clone()).taken = Some(event.t_ns),
                    trace::RELEASED => span.delivery(node, &input(), machine.clone()).released = Some(event.t_ns),
                    _ => {}
                };
                with_span(&mut inner.spans, &mut inner.order, &context, record);
            }
        }
    }

    /// Without `spans`, only the per-input latency.
    pub fn report(&self, spans: bool) -> TraceReport {
        self.collect();
        let inner = self.inner.lock().unwrap();
        let mut inputs = Vec::new();
        let mut dropped_events = 0;
        for (node, files) in &inner.files {
            dropped_events += files.events.as_ref().map_or(0, EventReader::dropped);
            let Some(stats) = &files.stats else { continue };
            for histograms in stats.inputs() {
                let Some((source, output)) = self.feeds.get(&(node.clone(), histograms.input.clone())) else {
                    continue;
                };
                let source_machine = self.machine_of.get(source).cloned();
                let clock_error_ns = if source_machine == self.machine {
                    Some(0)
                } else {
                    let error = |m: &Option<String>| Some(inner.clocks.get(m.as_ref()?)?.error_ns);
                    error(&source_machine).zip(error(&self.machine)).map(|(a, b)| a + b)
                };
                inputs.push(InputReport {
                    node: node.clone(),
                    input: histograms.input,
                    source: format!("{source}/{output}"),
                    machine: self.machine.clone(),
                    source_machine,
                    latency: histograms.latency.into(),
                    processing: histograms.processing.into(),
                    clock_error_ns,
                });
            }
        }
        let spans = match spans {
            true => inner.order.iter().filter_map(|id| inner.spans.get(id)).cloned().collect(),
            false => Vec::new(),
        };
        TraceReport { inputs, spans, dropped_events, clocks: inner.clocks.clone() }
    }
}

impl Inner {
    fn with_span(&mut self, context: &Context, f: impl FnOnce(&mut SpanRecord)) {
        with_span(&mut self.spans, &mut self.order, context, f);
    }
}

/// Finds or creates the record of `context.span`, forgetting the oldest
/// beyond `MAX_SPANS`.
fn with_span(
    spans: &mut HashMap<u64, SpanRecord>,
    order: &mut VecDeque<u64>,
    context: &Context,
    f: impl FnOnce(&mut SpanRecord),
) {
    if !spans.contains_key(&context.span) {
        if order.len() == MAX_SPANS {
            if let Some(oldest) = order.pop_front() {
                spans.remove(&oldest);
            }
        }
        order.push_back(context.span);
        spans.insert(context.span, SpanRecord { span: context.span, ..Default::default() });
    }
    let span = spans.get_mut(&context.span).unwrap();
    span.trace = context.trace;
    span.parent = context.parent;
    f(span);
}

impl SpanRecord {
    fn delivery(&mut self, node: &str, input: &str, machine: Option<String>) -> &mut Delivery {
        match self.deliveries.iter().position(|d| d.node == node && (d.input == input || d.input.is_empty())) {
            Some(i) => {
                let delivery = &mut self.deliveries[i];
                if delivery.input.is_empty() {
                    delivery.input = input.to_owned();
                }
                delivery
            }
            None => {
                self.deliveries.push(Delivery {
                    node: node.to_owned(),
                    input: input.to_owned(),
                    machine,
                    ..Default::default()
                });
                self.deliveries.last_mut().unwrap()
            }
        }
    }
}
