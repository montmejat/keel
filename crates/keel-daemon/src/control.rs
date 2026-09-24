//! The control API: what tools use to inspect and drive a running daemon.
//!
//! Newline-delimited JSON over `<runtime dir>/<pid>/control.sock`: one
//! request per line, one reply per line. It's meant to be poked at by hand:
//!
//! ```sh
//! echo '{"cmd":"status"}' | socat - UNIX-CONNECT:$XDG_RUNTIME_DIR/keel/<pid>/control.sock
//! ```

use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::runtime;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Status,
    /// Log lines numbered `since` and after, as far back as the daemon keeps.
    Logs {
        since: u64,
    },
    /// Stops the dataflow gracefully.
    Stop,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reply {
    Status(Status),
    Logs(Logs),
    Stopping,
    Error(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    pub pid: u32,
    pub dataflow: PathBuf,
    pub uptime_ms: u64,
    pub stopping: bool,
    pub nodes: Vec<NodeStatus>,
    pub links: Vec<LinkStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeStatus {
    pub id: String,
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
    /// `node/input`
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
