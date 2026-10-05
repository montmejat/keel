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
    /// Which protocol and which keel this is. A client asks first.
    Hello,
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
    Event(Event),
    Status(Status),
    Logs(Logs),
    Stopping,
    Trace(TraceReport),
    /// The nodes an update replaces.
    Updating(Vec<String>),
    Error(String),
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

/// Serves each connection on its own thread, answering with `handle`.
pub(crate) fn serve(listener: UnixListener, handle: impl Fn(Request) -> Reply + Send + Sync + 'static) {
    let handle = Arc::new(handle);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let handle = handle.clone();
            std::thread::spawn(move || {
                let _ = serve_connection(stream, &*handle);
            });
        }
    });
}

fn serve_connection(stream: UnixStream, handle: &dyn Fn(Request) -> Reply) -> io::Result<()> {
    let mut writer = stream.try_clone()?;
    for line in BufReader::new(stream).lines() {
        let reply = match serde_json::from_str(&line?) {
            Ok(Request::Subscribe { interval_ms }) => return stream_events(&mut writer, interval_ms, handle),
            Ok(request) => handle(request),
            Err(e) => Reply::Error(format!("invalid request: {e}")),
        };
        let mut out = serde_json::to_vec(&reply)?;
        out.push(b'\n');
        writer.write_all(&out)?;
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
        let stream = UnixStream::connect(runtime::control_socket(pid))
            .map_err(|e| io::Error::new(e.kind(), format!("can't reach the daemon with pid {pid}: {e}")))?;
        let mut client = Self { reader: BufReader::new(stream.try_clone()?), writer: stream };
        // A daemon from before there was a `hello` answers with an error.
        match client.request(&Request::Hello) {
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

/// The daemons running, each with its status. One may exit between being
/// listed and being asked, and is left out.
pub fn running() -> Vec<(u32, Status)> {
    (runtime::running_daemons().into_iter())
        .filter_map(|pid| Some((pid, Client::connect(pid).and_then(|mut c| c.status()).ok()?)))
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
        [(pid, _)] => Ok(*pid),
        _ => {
            let coordinators: Vec<u32> = all.iter().filter(|(_, s)| s.coordinator).map(|(p, _)| *p).collect();
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
            Request::Hello => hello(),
            Request::Status => Reply::Status(status()),
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
}
