//! The coordinator: `keel run` for a dataflow spread over machines.
//!
//! Sends each machine's daemon the dataflow, lets every node start once all of
//! them have registered, and relays stop requests so that all machines stop
//! together. It isn't on the data path: daemons send data to each other.
//!
//! It also serves the control API for the dataflow as a whole: it keeps every
//! machine's logs, and answers `status` and `trace` by asking every daemon
//! and merging their answers. Traces are put on its own clock, using each
//! machine's offset, measured NTP-style over the connection to its daemon.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::io::{self, BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use keel::trace;

use crate::control::{self, Clock, Delivery, LogLine, Logs, Reply, Request, SpanRecord, Status, TraceReport};
use crate::dataflow::Dataflow;
use crate::runtime::RuntimeDir;
use crate::signals;
use crate::wire::{self, Event, ToDaemon};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a control request waits for every daemon's answer.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(2);
/// Clock sync: a burst of pings at start, then one per second, keeping the
/// latest samples and trusting the one with the shortest round trip.
const FIRST_PINGS: u32 = 10;
const FIRST_PING_INTERVAL: Duration = Duration::from_millis(50);
const PING_INTERVAL: Duration = Duration::from_secs(1);
const CLOCK_SAMPLES: usize = 16;
const LOG_CAPACITY: usize = 10_000;

struct Coordinator {
    name: PathBuf,
    deployment: Option<String>,
    /// Each node's position in the dataflow, to list them in that order.
    order: HashMap<String, usize>,
    start: Instant,
    daemons: Mutex<BTreeMap<String, TcpStream>>,
    /// Control requests waiting for daemons' answers, by id.
    pending: Mutex<HashMap<u64, mpsc::Sender<(String, Reply)>>>,
    next_id: AtomicU64,
    clocks: Mutex<BTreeMap<String, ClockSamples>>,
    logs: Mutex<(VecDeque<LogLine>, u64)>,
    stop_requested: AtomicBool,
}

#[derive(Default)]
struct ClockSamples {
    /// `(round trip, offset)`, oldest first.
    samples: VecDeque<(u64, i64)>,
    best: Option<Clock>,
}

pub(crate) fn run(
    name: PathBuf,
    dataflow: Dataflow,
    base_dir: PathBuf,
    binaries: BTreeMap<String, String>,
    deployment: Option<String>,
) -> io::Result<bool> {
    let runtime = RuntimeDir::create()?;
    let control_listener = UnixListener::bind(runtime.control_socket())?;
    signals::install();

    let coordinator = Arc::new(Coordinator {
        name: name.clone(),
        deployment: deployment.clone(),
        order: dataflow.nodes.iter().enumerate().map(|(i, n)| (n.id.clone(), i)).collect(),
        start: Instant::now(),
        daemons: Mutex::new(BTreeMap::new()),
        pending: Mutex::new(HashMap::new()),
        next_id: AtomicU64::new(0),
        clocks: Mutex::new(BTreeMap::new()),
        logs: Mutex::new((VecDeque::new(), 0)),
        stop_requested: AtomicBool::new(false),
    });

    // Lifecycle events from every daemon, tagged with its machine; `None`
    // when its connection closes.
    let (events_tx, events) = mpsc::channel::<(String, Option<Event>)>();
    for (machine, address) in &dataflow.machines {
        let on_machine =
            |node: &String| dataflow.nodes.iter().any(|n| &n.id == node && n.machine.as_ref() == Some(machine));
        let spawn = ToDaemon::Spawn {
            name: name.clone(),
            machine: machine.clone(),
            dataflow: dataflow.clone(),
            base_dir: base_dir.clone(),
            binaries: binaries
                .iter()
                .filter(|(node, _)| on_machine(node))
                .map(|(n, h)| (n.clone(), h.clone()))
                .collect(),
            deployment: deployment.clone(),
        };
        let stream = connect(address).and_then(|mut stream| {
            stream.write_all(&[wire::COORDINATOR])?;
            wire::write_json(&mut stream, &spawn)?;
            Ok(stream)
        });
        let stream = match stream {
            Ok(stream) => stream,
            Err(e) => {
                coordinator.broadcast(&ToDaemon::Abort);
                return Err(io::Error::other(format!(
                    "can't reach the daemon of machine `{machine}` at {address}: {e}"
                )));
            }
        };
        let (mut reader, events_tx, machine_name, coordinator_) =
            (BufReader::new(stream.try_clone()?), events_tx.clone(), machine.clone(), coordinator.clone());
        std::thread::spawn(move || {
            while let Ok(Some(event)) = wire::read_json(&mut reader) {
                match event {
                    Event::Pong { t1, t2, t3 } => coordinator_.on_pong(&machine_name, t1, t2, t3, trace::now_ns()),
                    Event::Control { id, reply } => coordinator_.on_reply(&machine_name, id, reply),
                    Event::Log { line } => coordinator_.log(&format!("{}@{machine_name}", line.node), line.text),
                    event => {
                        let _ = events_tx.send((machine_name.clone(), Some(event)));
                    }
                }
            }
            let _ = events_tx.send((machine_name, None));
        });
        coordinator.daemons.lock().unwrap().insert(machine.clone(), stream);
    }
    {
        let coordinator = coordinator.clone();
        control::serve(control_listener, move |request| coordinator.handle(request));
    }
    let machines = dataflow.machines.len();
    coordinator.say(format!(
        "running {} nodes on {machines} machines, control socket {}",
        dataflow.nodes.len(),
        runtime.control_socket().display()
    ));

    let (mut registered, mut finished) = (BTreeSet::new(), BTreeSet::new());
    let (mut ok, mut stopping, mut aborting, mut signals_seen) = (true, false, false, 0);
    let (mut pings, mut next_ping) = (0, Instant::now());
    let abort = |aborting: &mut bool, reason: String| {
        if !std::mem::replace(aborting, true) {
            coordinator.say(format!("{reason}, aborting the dataflow"));
            coordinator.broadcast(&ToDaemon::Abort);
        }
    };
    while finished.len() < machines {
        if Instant::now() >= next_ping {
            coordinator.ping_all();
            pings += 1;
            next_ping += if pings < FIRST_PINGS { FIRST_PING_INTERVAL } else { PING_INTERVAL };
        }
        match events.recv_timeout(Duration::from_millis(20)) {
            Ok((machine, Some(event))) => match event {
                Event::AllRegistered => {
                    registered.insert(machine);
                    if registered.len() == machines {
                        coordinator.say("all nodes registered on all machines, starting".into());
                        coordinator.broadcast(&ToDaemon::Start);
                    }
                }
                Event::NodeExited { node, success: false } if !stopping => {
                    ok = false;
                    abort(&mut aborting, format!("`{node}` failed on `{machine}`"));
                }
                Event::NodeExited { success, .. } => ok &= success,
                Event::StopRequested if !stopping => {
                    stopping = true;
                    coordinator.say(format!("stop requested on `{machine}`, stopping all machines"));
                    coordinator.broadcast(&ToDaemon::Stop);
                }
                Event::Finished { ok: machine_ok } => {
                    ok &= machine_ok;
                    finished.insert(machine);
                }
                Event::Error { message } => {
                    ok = false;
                    // The daemon won't run its share: don't wait for it.
                    finished.insert(machine.clone());
                    abort(&mut aborting, format!("`{machine}`: {message}"));
                }
                _ => {}
            },
            Ok((machine, None)) => {
                if finished.insert(machine.clone()) {
                    ok = false;
                    abort(&mut aborting, format!("lost the connection to `{machine}`"));
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        let signals = signals::received();
        let stop = signals != signals_seen && signals == 1;
        if (stop || coordinator.stop_requested.swap(false, Ordering::Relaxed)) && !stopping {
            stopping = true;
            coordinator.say("stopping all machines".into());
            coordinator.broadcast(&ToDaemon::Stop);
        } else if signals != signals_seen && signals > 1 {
            ok = false;
            abort(&mut aborting, "interrupted again".into());
        }
        signals_seen = signals;
    }
    coordinator.say(format!("dataflow finished{}", if ok { "" } else { " with failures" }));
    Ok(ok)
}

impl Coordinator {
    fn broadcast(&self, message: &ToDaemon) {
        for stream in self.daemons.lock().unwrap().values_mut() {
            // A daemon we can't reach shows up as a closed connection.
            let _ = wire::write_json(stream, message);
        }
    }

    fn ping_all(&self) {
        for stream in self.daemons.lock().unwrap().values_mut() {
            let _ = wire::write_json(stream, &ToDaemon::Ping { t1: trace::now_ns() });
        }
    }

    /// One clock sample: `t1` and `t4` on our clock, `t2` and `t3` on the
    /// daemon's. Tells daemons the new offsets when the best sample changes.
    fn on_pong(&self, machine: &str, t1: u64, t2: u64, t3: u64, t4: u64) {
        let round_trip = (t4 - t1).saturating_sub(t3.saturating_sub(t2));
        let offset = ((t2 as i128 - t1 as i128) + (t3 as i128 - t4 as i128)) / 2;
        let mut clocks = self.clocks.lock().unwrap();
        let entry = clocks.entry(machine.to_owned()).or_default();
        entry.samples.push_back((round_trip, offset as i64));
        if entry.samples.len() > CLOCK_SAMPLES {
            entry.samples.pop_front();
        }
        let &(rtt, offset_ns) = entry.samples.iter().min_by_key(|(rtt, _)| *rtt).unwrap();
        let best = Clock { offset_ns, error_ns: rtt.div_ceil(2) };
        if entry.best != Some(best) {
            entry.best = Some(best);
            let all = clocks.iter().filter_map(|(m, c)| Some((m.clone(), c.best?))).collect();
            drop(clocks);
            self.broadcast(&ToDaemon::Clocks { clocks: all });
        }
    }

    fn clocks(&self) -> BTreeMap<String, Clock> {
        self.clocks.lock().unwrap().iter().filter_map(|(m, c)| Some((m.clone(), c.best?))).collect()
    }

    fn on_reply(&self, machine: &str, id: u64, reply: Reply) {
        if let Some(waiting) = self.pending.lock().unwrap().get(&id) {
            let _ = waiting.send((machine.to_owned(), reply));
        }
    }

    /// Asks every daemon, and returns the answers that came in time.
    fn ask_all(&self, request: Request) -> Vec<(String, Reply)> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let expected = self.daemons.lock().unwrap().len();
        self.broadcast(&ToDaemon::Control { id, request });
        let deadline = Instant::now() + CONTROL_TIMEOUT;
        let mut replies = Vec::new();
        while replies.len() < expected {
            match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(reply) => replies.push(reply),
                Err(_) => break,
            }
        }
        self.pending.lock().unwrap().remove(&id);
        replies
    }

    fn handle(&self, request: Request) -> Reply {
        match request {
            Request::Status => Reply::Status(self.status()),
            Request::Logs { since } => {
                let logs = self.logs.lock().unwrap();
                let lines = logs.0.iter().filter(|l| l.seq >= since).cloned().collect();
                Reply::Logs(Logs { lines, next: logs.1 })
            }
            Request::Stop => {
                self.stop_requested.store(true, Ordering::Relaxed);
                Reply::Stopping
            }
            Request::Trace { summary } => Reply::Trace(self.trace(summary)),
        }
    }

    fn status(&self) -> Status {
        let mut status = Status {
            pid: std::process::id(),
            machine: None,
            dataflow: Some(self.name.clone()),
            uptime_ms: self.start.elapsed().as_millis() as u64,
            stopping: false,
            nodes: Vec::new(),
            links: Vec::new(),
            coordinator: true,
            deployment: self.deployment.clone(),
        };
        // Node ids are unique across machines, and nodes carry their machine:
        // endpoints don't need `@machine` here.
        let plain = |endpoint: &str| endpoint.split('@').next().unwrap_or(endpoint).to_owned();
        for (_, reply) in self.ask_all(Request::Status) {
            if let Reply::Status(machine) = reply {
                status.stopping |= machine.stopping;
                status.nodes.extend(machine.nodes);
                status.links.extend(machine.links.into_iter().map(|mut link| {
                    link.source = plain(&link.source);
                    link.targets = link.targets.iter().map(|t| plain(t)).collect();
                    link
                }));
            }
        }
        status.nodes.sort_by_key(|n| self.order.get(&n.id).copied());
        status
    }

    /// Every machine's report, merged onto our clock.
    fn trace(&self, summary: bool) -> TraceReport {
        let clocks = self.clocks();
        let mut merged = TraceReport { clocks: clocks.clone(), ..Default::default() };
        let mut spans: BTreeMap<u64, SpanRecord> = BTreeMap::new();
        for (machine, reply) in self.ask_all(Request::Trace { summary }) {
            let Reply::Trace(report) = reply else { continue };
            merged.inputs.extend(report.inputs);
            merged.dropped_events += report.dropped_events;
            let offset = clocks.get(&machine).map_or(0, |c| c.offset_ns);
            for mut span in report.spans {
                span.shift(offset);
                match spans.get_mut(&span.span) {
                    Some(existing) => existing.merge(span),
                    None => {
                        spans.insert(span.span, span);
                    }
                }
            }
        }
        merged.spans = spans.into_values().collect();
        merged
    }

    fn log(&self, node: &str, text: String) {
        let _ = writeln!(io::stdout(), "[{node}] {text}");
        let mut logs = self.logs.lock().unwrap();
        if logs.0.len() == LOG_CAPACITY {
            logs.0.pop_front();
        }
        let line = LogLine { seq: logs.1, t_ms: self.start.elapsed().as_millis() as u64, node: node.to_owned(), text };
        logs.0.push_back(line);
        logs.1 += 1;
    }

    fn say(&self, text: String) {
        let _ = writeln!(io::stderr(), "[coordinator] {text}");
        let mut logs = self.logs.lock().unwrap();
        let line =
            LogLine { seq: logs.1, t_ms: self.start.elapsed().as_millis() as u64, node: "coordinator".into(), text };
        logs.0.push_back(line);
        logs.1 += 1;
    }
}

impl SpanRecord {
    /// From a machine's clock to the coordinator's: `offset` is that
    /// machine's clock minus ours.
    fn shift(&mut self, offset: i64) {
        let shift = |t: &mut u64| *t = (*t as i128 - offset as i128).max(0) as u64;
        self.published.iter_mut().chain(self.routed.iter_mut()).for_each(shift);
        self.net_sent.values_mut().chain(self.net_received.values_mut()).for_each(shift);
        for d in &mut self.deliveries {
            d.delivered.iter_mut().chain(d.taken.iter_mut()).chain(d.released.iter_mut()).for_each(shift);
        }
    }

    /// Adds what another machine saw of the same message.
    fn merge(&mut self, other: SpanRecord) {
        if self.source.is_empty() {
            (self.source, self.source_machine) = (other.source, other.source_machine);
        }
        // The sender's publish time beats a receiver's converted copy of it.
        if other.routed.is_some() || self.published.is_none() {
            self.published = other.published.or(self.published);
        }
        self.routed = self.routed.or(other.routed);
        self.net_sent.extend(other.net_sent);
        self.net_received.extend(other.net_received);
        for d in other.deliveries {
            match self.deliveries.iter_mut().find(|e| e.node == d.node && e.input == d.input) {
                Some(e) => {
                    let Delivery { delivered, taken, released, .. } = d;
                    e.delivered = e.delivered.or(delivered);
                    e.taken = e.taken.or(taken);
                    e.released = e.released.or(released);
                }
                None => self.deliveries.push(d),
            }
        }
    }
}

fn connect(address: &str) -> io::Result<TcpStream> {
    let mut last_error = io::Error::other("address resolves to nothing");
    for address in address.to_socket_addrs()? {
        match TcpStream::connect_timeout(&address, CONNECT_TIMEOUT) {
            Ok(stream) => {
                stream.set_nodelay(true)?;
                return Ok(stream);
            }
            Err(e) => last_error = e,
        }
    }
    Err(last_error)
}
