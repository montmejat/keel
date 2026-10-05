//! The control API: what tools use to inspect and drive a running daemon.
//!
//! Newline-delimited JSON over `<runtime dir>/<pid>/control.sock`: one
//! request per line, one reply per line. It's meant to be poked at by hand:
//!
//! ```sh
//! echo '{"cmd":"status"}' | socat - UNIX-CONNECT:$XDG_RUNTIME_DIR/keel/<pid>/control.sock
//! ```

use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::runtime;

/// The version of this protocol, in `docs/protocol.md`. It goes up when a
/// request or a reply changes in a way an older client would misread; a
/// field added with a default doesn't count.
pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    /// Which protocol and which keel this is. A client asks first. An agent
    /// says so (`agent`), and its requests that act wait for a human: see
    /// `Request::Approve`.
    Hello {
        #[serde(default)]
        agent: bool,
        /// Who is asking, for the record of actions.
        #[serde(default)]
        client: Option<String>,
    },
    Status,
    /// Log lines numbered `since` and after, as far back as the daemon keeps.
    Logs {
        since: u64,
    },
    /// Stops the dataflow gracefully.
    Stop,
    /// Latency per input, and the sampled traces still in memory, unless
    /// `summary` asks for the latency only.
    Trace {
        #[serde(default)]
        summary: bool,
    },
    /// Switch to a new deployment of the same dataflow: nodes whose binary
    /// changed are restarted with the new one, one at a time.
    Update {
        deployment: Option<String>,
        binaries: BTreeMap<String, String>,
    },
    /// Kills a node so that it starts again, as its restart policy would after
    /// a crash. A node that is `restart: never` is refused.
    Restart {
        node: String,
    },
    /// The actions agents asked for, waiting or decided, oldest first.
    Actions,
    /// Does what an agent asked for. Refused from an agent's connection.
    Approve {
        id: u64,
    },
    /// Drops what an agent asked for. Refused from an agent's connection.
    Deny {
        id: u64,
    },
    /// Turns the connection into a stream: after `subscribed`, the daemon
    /// sends an `event` every `interval_ms`, until the client hangs up. Such a
    /// connection takes no more requests.
    Subscribe {
        #[serde(default = "default_interval_ms")]
        interval_ms: u64,
    },
}

fn default_interval_ms() -> u64 {
    250
}

/// The fastest a stream is sampled.
const MIN_INTERVAL_MS: u64 = 50;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reply {
    Hello(Hello),
    /// The connection is a stream now, sampled every `interval_ms`.
    Subscribed { interval_ms: u64 },
    /// The node was killed and starts again.
    Restarted(String),
    /// An agent's request is waiting for a human: `Request::Actions` shows it
    /// by this id.
    Pending { id: u64 },
    Actions(Vec<ActionRecord>),
    /// The action was dropped.
    Denied,
    Event(Event),
    Status(Status),
    Logs(Logs),
    Stopping,
    Trace(TraceReport),
    /// The nodes an update replaces.
    Updating(Vec<String>),
    Error(String),
}

/// What an agent asked for, and what became of it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionRecord {
    pub id: u64,
    /// In words, for the person deciding.
    pub summary: String,
    /// Who asked: the `client` it said hello with.
    pub client: String,
    pub state: ActionState,
    /// What happened once approved.
    pub outcome: Option<String>,
    /// Milliseconds since this daemon began serving.
    pub asked_ms: u64,
    pub decided_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionState {
    Pending,
    Approved,
    Denied,
}

/// What a node's restart answers with, from a machine that doesn't run it.
pub const NO_SUCH_NODE: &str = "no node ";

pub(crate) fn served_by_the_connection() -> Reply {
    Reply::Error("this request is served by the connection, not the dataflow".into())
}

/// What a stream carries. Each sample sends the three, in this order.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Event {
    Status(Status),
    /// The lines since the last event: none, most of the time. Not sent when
    /// there are none.
    Logs(Logs),
    /// Latency per input, without the spans.
    Latency(TraceReport),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    pub protocol: u32,
    /// The keel build, for a human reading an error.
    pub keel: String,
}

/// What a daemon answers to `Request::Hello`.
pub fn hello() -> Reply {
    Reply::Hello(Hello { protocol: PROTOCOL_VERSION, keel: env!("CARGO_PKG_VERSION").to_owned() })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    pub pid: u32,
    /// Machine name, when the daemon runs part of a multi-machine dataflow.
    pub machine: Option<String>,
    /// `None` while a `keel daemon` waits for a dataflow.
    pub dataflow: Option<PathBuf>,
    /// Of the running dataflow.
    pub uptime_ms: u64,
    pub stopping: bool,
    pub nodes: Vec<NodeStatus>,
    pub links: Vec<LinkStatus>,
    /// A coordinator's view: every machine's nodes and links together.
    #[serde(default)]
    pub coordinator: bool,
    /// The deployment running, if the dataflow was deployed.
    #[serde(default)]
    pub deployment: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeStatus {
    pub id: String,
    /// Where it runs, for a multi-machine dataflow.
    #[serde(default)]
    pub machine: Option<String>,
    /// What it runs: the `build:` binary, or the `path:` file's name.
    #[serde(default)]
    pub program: String,
    pub pid: Option<u32>,
    pub state: NodeState,
    /// Times it was started again after exiting.
    #[serde(default)]
    pub restarts: u32,
    /// Shared-memory regions this node sends from.
    pub shm_regions: u32,
    /// Of those, the ones still being read.
    pub shm_held: u32,
    pub shm_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum NodeState {
    /// Spawned, waiting for every node to register.
    Starting,
    Running,
    /// Asked to stop, not exited yet.
    Stopping,
    Exited {
        success: bool,
        detail: String,
    },
}

/// One output, the inputs it feeds, and what went through it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkStatus {
    /// `node/output`
    pub source: String,
    /// `node/input`, or `node/input@machine` for a node on another machine.
    pub targets: Vec<String>,
    pub messages: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Logs {
    pub lines: Vec<LogLine>,
    /// Pass as `since` to get only newer lines.
    pub next: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogLine {
    pub seq: u64,
    /// Milliseconds since the daemon started.
    pub t_ms: u64,
    /// Node id, or `daemon`.
    pub node: String,
    pub text: String,
}

/// Where the time went, as one daemon or the coordinator saw it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TraceReport {
    pub inputs: Vec<InputReport>,
    /// Sampled messages, possibly partial: each machine only knows the hops
    /// it saw. Times are nanoseconds on one clock: the machine's own, or the
    /// coordinator's once it has merged the reports.
    pub spans: Vec<SpanRecord>,
    /// Trace events nodes couldn't record because their ring was full.
    pub dropped_events: u64,
    /// Each machine's clock minus the coordinator's, as last measured.
    pub clocks: BTreeMap<String, Clock>,
}

/// One input of one node: how long its messages took to arrive, and to
/// process.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputReport {
    pub node: String,
    pub input: String,
    /// `node/output` feeding it.
    pub source: String,
    pub machine: Option<String>,
    pub source_machine: Option<String>,
    /// Published → taken by the receiver.
    pub latency: Percentiles,
    /// Taken → released.
    pub processing: Percentiles,
    /// How far off `latency` may be because the two machines' clocks aren't
    /// perfectly aligned: 0 on one machine, `None` while not yet measured.
    pub clock_error_ns: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Percentiles {
    pub count: u64,
    pub p50: u64,
    pub p99: u64,
    pub p999: u64,
    pub max: u64,
}

impl From<keel::trace::Summary> for Percentiles {
    fn from(s: keel::trace::Summary) -> Self {
        Self { count: s.count, p50: s.p50, p99: s.p99, p999: s.p999, max: s.max }
    }
}

/// A machine's clock relative to the coordinator's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Clock {
    /// Machine clock minus coordinator clock.
    pub offset_ns: i64,
    /// The true offset is within this much of `offset_ns`.
    pub error_ns: u64,
}

/// One sampled message and what happened to it, from publish to every
/// receiver releasing it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SpanRecord {
    pub span: u64,
    pub trace: u64,
    /// 0 for the message that started the trace.
    pub parent: u64,
    /// `node/output`, once known.
    pub source: String,
    pub source_machine: Option<String>,
    pub published: Option<u64>,
    /// The sender's daemon read the descriptor.
    pub routed: Option<u64>,
    /// Written to the connection to each machine, by machine.
    pub net_sent: BTreeMap<String, u64>,
    /// Read from another machine, by the receiving machine.
    pub net_received: BTreeMap<String, u64>,
    pub deliveries: Vec<Delivery>,
}

/// One receiver of a span.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Delivery {
    pub node: String,
    pub input: String,
    pub machine: Option<String>,
    /// The daemon sent the descriptor to the node.
    pub delivered: Option<u64>,
    pub taken: Option<u64>,
    pub released: Option<u64>,
}

/// Requests that change something: from an agent, they wait for a human.
fn acts(request: &Request) -> bool {
    matches!(request, Request::Stop | Request::Update { .. } | Request::Restart { .. })
}

fn describe(request: &Request) -> String {
    match request {
        Request::Stop => "stop the dataflow".into(),
        Request::Restart { node } => format!("restart node `{node}`"),
        Request::Update { deployment, binaries } => format!(
            "roll {} in, replacing {} node binaries",
            deployment.as_deref().map_or("a new deployment".into(), |d| format!("deployment {d}")),
            binaries.len()
        ),
        other => format!("{other:?}"),
    }
}

/// An action's record, and the request it holds while it is pending.
type Action = (ActionRecord, Option<Request>);

/// The actions agents asked for, shared by a daemon's connections.
struct Actions {
    start: std::time::Instant,
    /// Oldest first.
    list: std::sync::Mutex<(u64, Vec<Action>)>,
}

/// How many decided actions are kept to show.
const KEEP_DECIDED: usize = 100;

impl Actions {
    fn now_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }

    fn ask(&self, request: Request, client: &str) -> u64 {
        let mut list = self.list.lock().unwrap();
        list.0 += 1;
        let id = list.0;
        let record = ActionRecord {
            id,
            summary: describe(&request),
            client: client.to_owned(),
            state: ActionState::Pending,
            outcome: None,
            asked_ms: self.now_ms(),
            decided_ms: None,
        };
        eprintln!("[daemon] {client} asks to {} (action {id}): `keel approve {id}` or `keel deny {id}`", record.summary);
        list.1.push((record, Some(request)));
        let decided = list.1.iter().filter(|(r, _)| r.state != ActionState::Pending).count();
        if decided > KEEP_DECIDED {
            let oldest = list.1.iter().position(|(r, _)| r.state != ActionState::Pending).unwrap();
            list.1.remove(oldest);
        }
        id
    }

    fn records(&self) -> Vec<ActionRecord> {
        self.list.lock().unwrap().1.iter().map(|(r, _)| r.clone()).collect()
    }

    /// Takes the request of a pending action, deciding it. The caller says
    /// what became of it with `finish`.
    fn take(&self, id: u64, state: ActionState) -> Result<Request, String> {
        let mut list = self.list.lock().unwrap();
        let Some((record, request)) = list.1.iter_mut().find(|(r, _)| r.id == id) else {
            return Err(format!("no action {id}"));
        };
        let Some(request) = request.take() else { return Err(format!("action {id} is already decided")) };
        record.state = state;
        record.decided_ms = Some(self.start.elapsed().as_millis() as u64);
        Ok(request)
    }

    fn finish(&self, id: u64, outcome: String) {
        if let Some((record, _)) = self.list.lock().unwrap().1.iter_mut().find(|(r, _)| r.id == id) {
            record.outcome = Some(outcome);
        }
    }
}

/// What a reply to an acting request means, in words.
pub fn outcome(reply: &Reply) -> String {
    match reply {
        Reply::Stopping => "stopping".into(),
        Reply::Restarted(node) => format!("`{node}` was killed and starts again"),
        Reply::Updating(nodes) => format!("replacing {}", nodes.join(", ")),
        Reply::Error(e) => format!("failed: {e}"),
        other => format!("{other:?}"),
    }
}

/// Serves each connection on its own thread, answering with `handle`.
///
/// A connection that says it is an agent can look at everything, but what it
/// asks that changes something waits as a pending action until a person
/// approves it from another connection. Everything else goes straight to
/// `handle`.
pub(crate) fn serve(listener: UnixListener, handle: impl Fn(Request) -> Reply + Send + Sync + 'static) {
    let handle = Arc::new(handle);
    let actions = Arc::new(Actions { start: std::time::Instant::now(), list: Default::default() });
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let (handle, actions) = (handle.clone(), actions.clone());
            std::thread::spawn(move || {
                let _ = serve_connection(stream, &*handle, &actions);
            });
        }
    });
}

fn serve_connection(stream: UnixStream, handle: &dyn Fn(Request) -> Reply, actions: &Actions) -> io::Result<()> {
    let mut writer = stream.try_clone()?;
    let (mut agent, mut client) = (false, String::from("an agent"));
    for line in BufReader::new(stream).lines() {
        let reply = match serde_json::from_str(&line?) {
            Ok(Request::Subscribe { interval_ms }) => return stream_events(&mut writer, interval_ms, handle),
            Ok(request) => match request {
                Request::Hello { agent: is_agent, client: who } => {
                    // Once an agent, always: saying hello again can't take it back.
                    agent |= is_agent;
                    client = who.unwrap_or(client);
                    handle(Request::Hello { agent: is_agent, client: None })
                }
                Request::Actions => Reply::Actions(actions.records()),
                Request::Approve { .. } | Request::Deny { .. } if agent => {
                    Reply::Error("an agent can't decide on actions, only ask for them".into())
                }
                Request::Approve { id } => match actions.take(id, ActionState::Approved) {
                    Ok(request) => {
                        let reply = handle(request);
                        eprintln!("[daemon] action {id} approved: {}", outcome(&reply));
                        actions.finish(id, outcome(&reply));
                        reply
                    }
                    Err(e) => Reply::Error(e),
                },
                Request::Deny { id } => match actions.take(id, ActionState::Denied) {
                    Ok(_) => {
                        eprintln!("[daemon] action {id} denied");
                        Reply::Denied
                    }
                    Err(e) => Reply::Error(e),
                },
                request if agent && acts(&request) => Reply::Pending { id: actions.ask(request, &client) },
                request => handle(request),
            },
            Err(e) => Reply::Error(format!("invalid request: {e}")),
        };
        send(&mut writer, &reply)?;
    }
    Ok(())
}

fn send(writer: &mut UnixStream, reply: &Reply) -> io::Result<()> {
    let mut out = serde_json::to_vec(reply)?;
    out.push(b'\n');
    writer.write_all(&out)
}

/// Samples `handle` every `interval_ms` and sends what it answers, until the
/// client is gone or the daemon stops answering. It is the same questions a
/// client would ask, asked here, so the daemon, a session and a coordinator
/// all stream without knowing it.
fn stream_events(writer: &mut UnixStream, interval_ms: u64, handle: &dyn Fn(Request) -> Reply) -> io::Result<()> {
    let interval_ms = interval_ms.max(MIN_INTERVAL_MS);
    send(writer, &Reply::Subscribed { interval_ms })?;
    let mut since = 0;
    loop {
        let (Reply::Status(status), Reply::Logs(logs), Reply::Trace(latency)) =
            (handle(Request::Status), handle(Request::Logs { since }), handle(Request::Trace { summary: true }))
        else {
            return Ok(());
        };
        since = logs.next;
        send(writer, &Reply::Event(Event::Status(status)))?;
        if !logs.lines.is_empty() {
            send(writer, &Reply::Event(Event::Logs(logs)))?;
        }
        send(writer, &Reply::Event(Event::Latency(latency)))?;
        std::thread::sleep(std::time::Duration::from_millis(interval_ms));
    }
}

/// A connection to a daemon's control API.
pub struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Client {
    pub fn connect(pid: u32) -> io::Result<Self> {
        Self::connect_as(pid, false, None)
    }

    /// As an agent: what it asks that changes something waits for a person
    /// to approve it (`keel approve`), and it can't approve anything itself.
    pub fn connect_as_agent(pid: u32, client: &str) -> io::Result<Self> {
        Self::connect_as(pid, true, Some(client.to_owned()))
    }

    fn connect_as(pid: u32, agent: bool, who: Option<String>) -> io::Result<Self> {
        let stream = UnixStream::connect(runtime::control_socket(pid))
            .map_err(|e| io::Error::new(e.kind(), format!("can't reach the daemon with pid {pid}: {e}")))?;
        let mut client = Self { reader: BufReader::new(stream.try_clone()?), writer: stream };
        // A daemon from before there was a `hello` answers with an error.
        match client.request(&Request::Hello { agent, client: who }) {
            Ok(Reply::Hello(Hello { protocol: PROTOCOL_VERSION, .. })) => Ok(client),
            Ok(Reply::Hello(hello)) => Err(io::Error::other(format!(
                "the daemon with pid {pid} (keel {}) speaks control protocol {}, this tool {PROTOCOL_VERSION}",
                hello.keel, hello.protocol
            ))),
            _ => Err(io::Error::other(format!(
                "the daemon with pid {pid} is from an older keel and speaks control protocol 0, this tool {PROTOCOL_VERSION}"
            ))),
        }
    }

    pub fn request(&mut self, request: &Request) -> io::Result<Reply> {
        let mut out = serde_json::to_vec(request)?;
        out.push(b'\n');
        self.writer.write_all(&out)?;
        let mut line = String::new();
        if self.reader.read_line(&mut line)? == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the daemon closed the connection"));
        }
        match serde_json::from_str(&line)? {
            Reply::Error(e) => Err(io::Error::other(e)),
            reply => Ok(reply),
        }
    }

    pub fn status(&mut self) -> io::Result<Status> {
        match self.request(&Request::Status)? {
            Reply::Status(status) => Ok(status),
            other => Err(unexpected(other)),
        }
    }

    pub fn logs(&mut self, since: u64) -> io::Result<Logs> {
        match self.request(&Request::Logs { since })? {
            Reply::Logs(logs) => Ok(logs),
            other => Err(unexpected(other)),
        }
    }

    pub fn trace(&mut self) -> io::Result<TraceReport> {
        self.trace_report(false)
    }

    /// Latency per input only, without the spans: cheap enough to poll.
    pub fn latency(&mut self) -> io::Result<TraceReport> {
        self.trace_report(true)
    }

    fn trace_report(&mut self, summary: bool) -> io::Result<TraceReport> {
        match self.request(&Request::Trace { summary })? {
            Reply::Trace(report) => Ok(report),
            other => Err(unexpected(other)),
        }
    }

    pub fn update(
        &mut self,
        deployment: Option<String>,
        binaries: BTreeMap<String, String>,
    ) -> io::Result<Vec<String>> {
        match self.request(&Request::Update { deployment, binaries })? {
            Reply::Updating(nodes) => Ok(nodes),
            other => Err(unexpected(other)),
        }
    }

    /// Turns this connection into a stream of events, sampled every
    /// `interval`.
    pub fn subscribe(mut self, interval: std::time::Duration) -> io::Result<Subscription> {
        let interval_ms = interval.as_millis() as u64;
        match self.request(&Request::Subscribe { interval_ms })? {
            Reply::Subscribed { .. } => Ok(Subscription { reader: self.reader }),
            other => Err(unexpected(other)),
        }
    }

    /// Asks to kill a node so that it starts again. For an agent's connection
    /// the answer is `Pending`: the id of the action a person has to approve.
    pub fn restart(&mut self, node: &str) -> io::Result<Reply> {
        self.request(&Request::Restart { node: node.to_owned() })
    }

    pub fn actions(&mut self) -> io::Result<Vec<ActionRecord>> {
        match self.request(&Request::Actions)? {
            Reply::Actions(actions) => Ok(actions),
            other => Err(unexpected(other)),
        }
    }

    /// Does what an agent asked for, and says what became of it.
    pub fn approve(&mut self, id: u64) -> io::Result<Reply> {
        self.request(&Request::Approve { id })
    }

    pub fn deny(&mut self, id: u64) -> io::Result<()> {
        match self.request(&Request::Deny { id })? {
            Reply::Denied => Ok(()),
            other => Err(unexpected(other)),
        }
    }

    pub fn stop(&mut self) -> io::Result<()> {
        match self.request(&Request::Stop)? {
            Reply::Stopping => Ok(()),
            other => Err(unexpected(other)),
        }
    }
}

fn unexpected(reply: Reply) -> io::Error {
    io::Error::other(format!("unexpected reply from daemon: {reply:?}"))
}

/// The events of a subscribed connection, in order.
pub struct Subscription {
    reader: BufReader<UnixStream>,
}

impl Subscription {
    /// Waits for the next event. An error when the daemon is gone.
    pub fn next_event(&mut self) -> io::Result<Event> {
        let mut line = String::new();
        if self.reader.read_line(&mut line)? == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the daemon closed the connection"));
        }
        match serde_json::from_str(&line)? {
            Reply::Event(event) => Ok(event),
            Reply::Error(e) => Err(io::Error::other(e)),
            other => Err(unexpected(other)),
        }
    }
}

/// The daemons running, each with its status, or why it couldn't be had: a
/// daemon from another keel, which speaks another protocol, is listed with
/// that. One that exits between being listed and being asked is left out.
pub fn running() -> Vec<(u32, Result<Status, String>)> {
    (runtime::running_daemons().into_iter())
        .filter_map(|pid| match Client::connect(pid).and_then(|mut c| c.status()) {
            Ok(status) => Some((pid, Ok(status))),
            Err(_) if !runtime::control_socket(pid).exists() => None,
            Err(e) => Some((pid, Err(e.to_string()))),
        })
        .collect()
}

/// A running daemon in a line, for a list to choose from.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Summary {
    pub pid: u32,
    /// Why nothing more is known, e.g. a daemon from another keel.
    pub error: Option<String>,
    pub dataflow: Option<PathBuf>,
    pub uptime_ms: u64,
    pub nodes: usize,
    pub nodes_running: usize,
    pub machine: Option<String>,
    pub coordinator: bool,
}

/// Every running daemon, summed up.
pub fn summaries() -> Vec<Summary> {
    (running().into_iter())
        .map(|(pid, status)| match status {
            Ok(s) => Summary {
                pid,
                error: None,
                nodes_running: s.nodes.iter().filter(|n| n.state == NodeState::Running).count(),
                nodes: s.nodes.len(),
                dataflow: s.dataflow,
                uptime_ms: s.uptime_ms,
                machine: s.machine,
                coordinator: s.coordinator,
            },
            Err(e) => Summary {
                pid,
                error: Some(e),
                dataflow: None,
                uptime_ms: 0,
                nodes: 0,
                nodes_running: 0,
                machine: None,
                coordinator: false,
            },
        })
        .collect()
}

/// The daemon to talk to: `pid`, else the only one running, else the only
/// coordinator (which sees every machine).
pub fn pick(pid: Option<u32>) -> Result<u32, String> {
    if let Some(pid) = pid {
        return Ok(pid);
    }
    let all = running();
    match &all[..] {
        [] => Err("no dataflow or daemon is running".into()),
        [(pid, Ok(_))] => Ok(*pid),
        [(_, Err(e))] => Err(e.clone()),
        _ => {
            let coordinators: Vec<u32> =
                all.iter().filter(|(_, s)| s.as_ref().is_ok_and(|s| s.coordinator)).map(|(p, _)| *p).collect();
            if let [pid] = coordinators[..] {
                return Ok(pid);
            }
            let pids: Vec<String> = all.iter().map(|(p, _)| p.to_string()).collect();
            Err(format!("several dataflows are running ({}); pick one with --pid", pids.join(", ")))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use super::*;

    fn status() -> Status {
        Status {
            pid: 1,
            machine: None,
            dataflow: None,
            uptime_ms: 0,
            stopping: false,
            nodes: Vec::new(),
            links: Vec::new(),
            coordinator: false,
            deployment: None,
        }
    }

    /// A daemon that has a log line to give each time it is asked.
    fn serve_stub(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("keel-control-test-{}-{name}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let lines = AtomicU64::new(0);
        serve(UnixListener::bind(&path).unwrap(), move |request| match request {
            Request::Hello { .. } => hello(),
            Request::Status => Reply::Status(status()),
            Request::Stop => Reply::Stopping,
            Request::Restart { node } => Reply::Restarted(node),
            Request::Logs { since } => {
                let next = lines.fetch_add(1, Ordering::SeqCst) + 1;
                let line = LogLine { seq: since, t_ms: 0, node: "n".into(), text: format!("line {next}") };
                Reply::Logs(Logs { lines: vec![line], next: since + 1 })
            }
            Request::Trace { .. } => Reply::Trace(TraceReport::default()),
            _ => Reply::Error("not in this test".into()),
        });
        path
    }

    fn connect(path: &PathBuf) -> UnixStream {
        UnixStream::connect(path).unwrap()
    }

    #[test]
    fn a_subscription_streams_status_logs_and_latency_in_turn() {
        let path = serve_stub("stream");
        let mut stream = connect(&path);
        stream.write_all(b"{\"cmd\":\"subscribe\",\"interval_ms\":50}\n").unwrap();
        let mut reader = BufReader::new(stream);
        let mut next = || {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            serde_json::from_str::<Reply>(&line).unwrap()
        };
        assert!(matches!(next(), Reply::Subscribed { interval_ms: 50 }));
        // Two samples: each is a status, the new lines, the latency.
        for expected in [1, 2] {
            assert!(matches!(next(), Reply::Event(Event::Status(_))));
            let Reply::Event(Event::Logs(logs)) = next() else { panic!("expected logs") };
            assert_eq!(logs.next, expected, "each sample asks for the lines after the last one it sent");
            assert!(matches!(next(), Reply::Event(Event::Latency(_))));
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_stream_is_never_faster_than_the_minimum() {
        let path = serve_stub("min");
        let mut stream = connect(&path);
        stream.write_all(b"{\"cmd\":\"subscribe\",\"interval_ms\":1}\n").unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        let Reply::Subscribed { interval_ms } = serde_json::from_str(&line).unwrap() else { panic!() };
        assert_eq!(interval_ms, MIN_INTERVAL_MS);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn hello_names_the_protocol() {
        let path = serve_stub("hello");
        let mut stream = connect(&path);
        stream.write_all(b"{\"cmd\":\"hello\"}\n").unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        assert!(matches!(serde_json::from_str(&line).unwrap(), Reply::Hello(Hello { protocol: PROTOCOL_VERSION, .. })));
        let _ = std::fs::remove_file(&path);
    }

    /// What a connection sends and gets back, line by line.
    struct Talk(BufReader<UnixStream>, UnixStream);

    impl Talk {
        fn to(path: &PathBuf, hello: &str) -> Self {
            let stream = connect(path);
            let mut talk = Self(BufReader::new(stream.try_clone().unwrap()), stream);
            talk.say(hello);
            talk
        }

        fn say(&mut self, request: &str) -> Reply {
            writeln!(self.1, "{request}").unwrap();
            let mut line = String::new();
            self.0.read_line(&mut line).unwrap();
            serde_json::from_str(&line).unwrap()
        }
    }

    #[test]
    fn what_an_agent_asks_to_change_waits_for_a_person() {
        let path = serve_stub("gate");
        let mut agent = Talk::to(&path, r#"{"cmd":"hello","agent":true,"client":"test agent"}"#);
        let mut person = Talk::to(&path, r#"{"cmd":"hello"}"#);

        // Looking is free; asking to change is held.
        assert!(matches!(agent.say(r#"{"cmd":"status"}"#), Reply::Status(_)));
        let Reply::Pending { id } = agent.say(r#"{"cmd":"restart","node":"filter"}"#) else { panic!("held") };
        let Reply::Actions(list) = person.say(r#"{"cmd":"actions"}"#) else { panic!() };
        assert_eq!((list[0].state, list[0].client.as_str()), (ActionState::Pending, "test agent"));
        assert_eq!(list[0].summary, "restart node `filter`");

        // It can't decide for itself.
        assert!(matches!(agent.say(&format!(r#"{{"cmd":"approve","id":{id}}}"#)), Reply::Error(_)));
        // A person approving runs it, and the record says what happened.
        let reply = person.say(&format!(r#"{{"cmd":"approve","id":{id}}}"#));
        assert!(matches!(reply, Reply::Restarted(ref n) if n == "filter"));
        let Reply::Actions(list) = agent.say(r#"{"cmd":"actions"}"#) else { panic!() };
        assert_eq!(list[0].state, ActionState::Approved);
        assert_eq!(list[0].outcome.as_deref(), Some("`filter` was killed and starts again"));
        // Once only.
        assert!(matches!(person.say(&format!(r#"{{"cmd":"approve","id":{id}}}"#)), Reply::Error(_)));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_agent_stays_one() {
        let path = serve_stub("sticky");
        let mut agent = Talk::to(&path, r#"{"cmd":"hello","agent":true}"#);
        // Saying hello again as a person doesn't make it one.
        assert!(matches!(agent.say(r#"{"cmd":"hello","agent":false}"#), Reply::Hello(_)));
        assert!(matches!(agent.say(r#"{"cmd":"stop"}"#), Reply::Pending { .. }));
        assert!(matches!(agent.say(r#"{"cmd":"approve","id":1}"#), Reply::Error(_)));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_denied_action_never_runs() {
        let path = serve_stub("deny");
        let mut agent = Talk::to(&path, r#"{"cmd":"hello","agent":true}"#);
        let mut person = Talk::to(&path, r#"{"cmd":"hello"}"#);
        let Reply::Pending { id } = agent.say(r#"{"cmd":"stop"}"#) else { panic!("held") };
        assert!(matches!(person.say(&format!(r#"{{"cmd":"deny","id":{id}}}"#)), Reply::Denied));
        let Reply::Actions(list) = person.say(r#"{"cmd":"actions"}"#) else { panic!() };
        assert_eq!((list[0].state, list[0].outcome.as_deref()), (ActionState::Denied, None));
        assert!(matches!(person.say(&format!(r#"{{"cmd":"approve","id":{id}}}"#)), Reply::Error(_)));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_person_acts_directly() {
        let path = serve_stub("direct");
        let mut person = Talk::to(&path, r#"{"cmd":"hello"}"#);
        assert!(matches!(person.say(r#"{"cmd":"stop"}"#), Reply::Stopping));
        let Reply::Actions(list) = person.say(r#"{"cmd":"actions"}"#) else { panic!() };
        assert!(list.is_empty());
        let _ = std::fs::remove_file(&path);
    }
}
