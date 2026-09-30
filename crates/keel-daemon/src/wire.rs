//! What the coordinator and the daemons say to each other, as JSON lines over
//! TCP.
//!
//! A TCP connection to a daemon starts with one byte saying what it is:
//! [`COORDINATOR`], then JSON lines both ways; [`PEER`], then a stream of
//! `keel::protocol::PeerMsg` frames carrying data between machines; or
//! [`BLOB`], then a [`BlobHeader`] line and a binary for the store, answered
//! with one [`Event`] line.

use std::collections::BTreeMap;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;

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
