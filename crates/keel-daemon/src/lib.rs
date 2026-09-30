//! keel-daemon: runs dataflows.
//!
//! - [`run`] runs a dataflow file. When everything is on this machine, it
//!   runs it in-process. When the dataflow lists machines, it acts as the
//!   coordinator of the `keel daemon`s running there.
//! - [`serve`] is `keel daemon`: waits for a coordinator and runs this
//!   machine's share of its dataflow.
//!
//! Either way, a [`session::Session`] does the actual work on each machine.

pub mod control;
mod coordinator;
mod daemon;
pub mod dataflow;
pub mod runtime;
mod session;
mod signals;
mod tracing;
pub mod wire;

use std::io;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

pub use daemon::serve;
use dataflow::Dataflow;
use runtime::{RuntimeDir, SessionFiles};
use session::{Session, SessionConfig};
use wire::Event;

/// Runs the dataflow at `path` until every node has exited. Returns whether
/// all of them succeeded.
pub fn run(path: &Path) -> io::Result<bool> {
    let dataflow = Dataflow::load(path)?;
    dataflow.resolve()?;
    if dataflow.machines.is_empty() {
        run_local(path, dataflow)
    } else {
        coordinator::run(path, dataflow)
    }
}

/// Plays both roles in one process: coordinator and the only daemon.
fn run_local(path: &Path, dataflow: Dataflow) -> io::Result<bool> {
    let runtime = RuntimeDir::create()?;
    let control_listener = UnixListener::bind(runtime.control_socket())?;
    signals::install();

    let name = std::fs::canonicalize(path)?;
    let base_dir = name.parent().unwrap().to_owned();
    let (events_tx, events) = mpsc::channel();
    let config = SessionConfig { name, machine: None, dataflow, base_dir };
    let session = Session::launch(config, SessionFiles::create(&runtime.dir)?, events_tx)?;
    {
        let session = session.clone();
        control::serve(control_listener, move |request| session.handle(request));
    }
    session.log("daemon", format!("pid {}, control socket {}", std::process::id(), runtime.control_socket().display()));

    let mut signals_seen = 0;
    loop {
        match events.recv_timeout(Duration::from_millis(20)) {
            Ok(Event::AllRegistered) => session.start()?,
            Ok(Event::StopRequested) => session.stop(),
            Ok(Event::Finished { ok }) => return Ok(ok),
            Ok(_) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Ok(false),
        }
        let signals = signals::received();
        if signals != signals_seen {
            signals_seen = signals;
            if signals == 1 {
                session.stop()
            } else {
                session.abort()
            }
        }
    }
}
