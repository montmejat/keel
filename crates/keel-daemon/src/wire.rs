//! What the coordinator and the daemons say to each other, as JSON lines over
//! TCP.
//!
//! A TCP connection to a daemon starts with one byte saying what it is:
//! [`COORDINATOR`], then JSON lines both ways, or [`PEER`], then a stream of
//! `keel::protocol::PeerMsg` frames carrying data between machines.

use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::control::LogLine;
use crate::dataflow::Dataflow;

pub const COORDINATOR: u8 = b'C';
pub const PEER: u8 = b'D';
pub const DEFAULT_LISTEN: &str = "127.0.0.1:7400";

/// Coordinator -> daemon.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "msg", rename_all = "snake_case")]
pub enum ToDaemon {
    /// Run this machine's share of the dataflow. Node paths are resolved
    /// against `base_dir` on the daemon's machine, so binaries must already
    /// be there (until packaging and deployment exist).
    Spawn { name: PathBuf, machine: String, dataflow: Dataflow, base_dir: PathBuf },
    /// Every node on every machine has registered: let them run.
    Start,
    /// Stop gracefully, draining from the sources.
    Stop,
    /// Kill everything now.
    Abort,
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
