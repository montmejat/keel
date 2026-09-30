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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Status,
    /// Log lines numbered `since` and after, as far back as the daemon keeps.
    Logs {
        since: u64,
    },
    /// Stops the dataflow gracefully.
    Stop,
    /// Latency per input, and the sampled traces still in memory.
    Trace,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reply {
    Status(Status),
    Logs(Logs),
    Stopping,
    Trace(TraceReport),
    Error(String),
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeStatus {
    pub id: String,
    /// Where it runs, for a multi-machine dataflow.
    #[serde(default)]
    pub machine: Option<String>,
    pub pid: Option<u32>,
    pub state: NodeState,
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
            Ok(request) => handle(request),
            Err(e) => Reply::Error(format!("invalid request: {e}")),
        };
        let mut out = serde_json::to_vec(&reply)?;
        out.push(b'\n');
        writer.write_all(&out)?;
    }
    Ok(())
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
        Ok(Self { reader: BufReader::new(stream.try_clone()?), writer: stream })
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
        match self.request(&Request::Trace)? {
            Reply::Trace(report) => Ok(report),
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
