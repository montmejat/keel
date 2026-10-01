//! What the coordinator and the daemons say to each other, as JSON lines over
//! TCP.
//!
//! A TCP connection to a daemon starts with one byte saying what it is, then
//! the cluster token on a line (see [`open`]). Then, for [`COORDINATOR`],
//! JSON lines both ways; for [`PEER`], a stream of `keel::protocol::PeerMsg`
//! frames carrying data between machines; for [`BLOB`], a [`BlobHeader`]
//! line and a binary for the store, answered with one [`Event`] line.
//!
//! The token is a shared secret that `keel provision` puts on every machine
//! (`~/.config/keel/token`). A daemon that has one refuses connections that
//! don't present it. It authenticates, it doesn't encrypt: on an untrusted
//! network, run keel over a VPN (WireGuard) or SSH tunnels.

use std::collections::BTreeMap;
use std::io::{self, BufRead, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::control::{Clock, LogLine, Reply, Request};
use crate::dataflow::Dataflow;

pub const COORDINATOR: u8 = b'C';
pub const PEER: u8 = b'D';
pub const BLOB: u8 = b'B';

/// Precedes a binary sent to a daemon's store.
#[derive(Debug, Serialize, Deserialize)]
pub struct BlobHeader {
    pub hash: String,
    pub len: u64,
}
pub const DEFAULT_LISTEN: &str = "127.0.0.1:7400";

/// Coordinator -> daemon.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "msg", rename_all = "snake_case")]
pub enum ToDaemon {
    /// Run this machine's share of the dataflow. Built nodes run the binary
    /// with the hash `binaries` gives, from the daemon's store; `path:` nodes
    /// are resolved against `base_dir` on the daemon's machine.
    Spawn {
        name: PathBuf,
        machine: String,
        dataflow: Dataflow,
        base_dir: PathBuf,
        #[serde(default)]
        binaries: BTreeMap<String, String>,
        #[serde(default)]
        deployment: Option<String>,
    },
    /// What platform are you, what do you hold? Answered with `Hello`.
    Hello,
    /// Which of these binaries are you missing?
    Missing { hashes: Vec<String> },
    /// Deployment `id` uses these binaries: keep them.
    Pin { id: String, hashes: Vec<String> },
    /// These deployments are gone: delete binaries nothing else uses.
    Unpin { ids: Vec<String> },
    /// Every node on every machine has registered: let them run.
    Start,
    /// Stop gracefully, draining from the sources.
    Stop,
    /// Kill everything now.
    Abort,
    /// Clock sync: `t1` is the coordinator's clock when sending.
    Ping { t1: u64 },
    /// Every machine's clock relative to the coordinator's, as measured.
    Clocks { clocks: BTreeMap<String, Clock> },
    /// A control API request, answered with [`Event::Control`].
    Control { id: u64, request: Request },
}

/// Daemon -> coordinator. Also what a session reports to `keel run` when
/// everything is local.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "msg", rename_all = "snake_case")]
pub enum Event {
    /// Every local node has registered.
    AllRegistered,
    NodeExited {
        node: String,
        success: bool,
    },
    /// Someone asked this daemon to stop the dataflow (`keel stop`, `keel top`).
    StopRequested,
    Log {
        line: LogLine,
    },
    /// Every local node has exited.
    Finished {
        ok: bool,
    },
    /// This daemon can't run its share of the dataflow.
    Error {
        message: String,
    },
    /// Answers `Hello`.
    Hello {
        /// The target to build this machine's binaries for.
        target: String,
        /// Binaries in the store, and their bytes.
        blobs: u64,
        bytes: u64,
    },
    Missing {
        hashes: Vec<String>,
    },
    /// A request succeeded.
    Done,
    /// Answers `Unpin`: what the store deleted.
    Collected {
        blobs: u64,
        bytes: u64,
    },
    /// Answers `Ping`: `t2` and `t3` are the daemon's clock when it received
    /// the ping and when it answered.
    Pong {
        t1: u64,
        t2: u64,
        t3: u64,
    },
    Control {
        id: u64,
        reply: Reply,
    },
}

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// `$XDG_CONFIG_HOME/keel/token`, or `~/.config/keel/token`.
pub fn token_path() -> PathBuf {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    std::env::var_os("XDG_CONFIG_HOME").map_or(home.join(".config"), PathBuf::from).join("keel/token")
}

pub fn load_token() -> Option<String> {
    let token = std::fs::read_to_string(token_path()).ok()?;
    Some(token.trim().to_owned()).filter(|t| !t.is_empty())
}

/// This machine's token, created (256 random bits, readable by us only) if
/// there isn't one yet.
pub fn ensure_token() -> io::Result<String> {
    if let Some(token) = load_token() {
        return Ok(token);
    }
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let path = token_path();
    std::fs::create_dir_all(path.parent().unwrap())?;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path)?;
    file.write_all(format!("{token}\n").as_bytes())?;
    Ok(token)
}

/// Connects to a daemon for a connection of `kind`, presenting our token.
pub fn open(address: &str, kind: u8) -> io::Result<TcpStream> {
    let mut last_error = io::Error::other("address resolves to nothing");
    for addr in address.to_socket_addrs()? {
        match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
            Ok(mut stream) => {
                stream.set_nodelay(true)?;
                stream.write_all(&[kind])?;
                stream.write_all(format!("{}\n", load_token().unwrap_or_default()).as_bytes())?;
                return Ok(stream);
            }
            Err(e) => last_error = e,
        }
    }
    Err(io::Error::new(last_error.kind(), format!("can't reach the daemon at {address}: {last_error}")))
}

/// The daemon's side: reads the token line and checks it against `expected`.
pub fn check_token(stream: &mut TcpStream, expected: Option<&str>) -> io::Result<()> {
    // Byte by byte, bounded: whatever follows belongs to the connection.
    let mut line = Vec::new();
    let mut byte = [0u8];
    loop {
        stream.read_exact(&mut byte)?;
        if byte[0] == b'\n' || line.len() > 128 {
            break;
        }
        line.push(byte[0]);
    }
    let Some(expected) = expected else { return Ok(()) };
    // Constant time: how much matched mustn't show in how long it took.
    let same = line.len() == expected.len() && line.iter().zip(expected.bytes()).fold(0, |d, (a, b)| d | (a ^ b)) == 0;
    match same {
        true => Ok(()),
        false => Err(io::Error::new(io::ErrorKind::PermissionDenied, "wrong or missing token")),
    }
}

pub fn write_json(w: &mut impl Write, value: &impl Serialize) -> io::Result<()> {
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    w.write_all(&line)
}

/// `Ok(None)` on a clean end of stream.
pub fn read_json<T: DeserializeOwned>(r: &mut impl BufRead) -> io::Result<Option<T>> {
    let mut line = String::new();
    match r.read_line(&mut line)? {
        0 => Ok(None),
        _ => Ok(Some(serde_json::from_str(&line)?)),
    }
}
