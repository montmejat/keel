//! keel-daemon: runs dataflows.
//!
//! - [`run`] runs a dataflow file, deploying it first if it has nodes to
//!   build (see [`packaging`]). When everything is on this machine, it runs
//!   in-process. When the dataflow lists machines, it acts as the
//!   coordinator of the `keel daemon`s running there.
//! - [`start`] runs a recorded deployment again, e.g. after a rollback.
//! - [`serve`] is `keel daemon`: waits for a coordinator and runs this
//!   machine's share of its dataflow.
//!
//! Either way, a [`session::Session`] does the actual work on each machine.

pub mod control;
mod coordinator;
mod daemon;
pub mod dataflow;
pub mod packaging;
pub mod provision;
pub mod runtime;
mod session;
mod sha256;
mod signals;
pub mod store;
mod tracing;
pub mod wire;

use std::collections::BTreeMap;
use std::io;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

pub use daemon::serve;
use dataflow::Dataflow;
use packaging::Deployment;
use runtime::{RuntimeDir, SessionFiles};
use session::{Session, SessionConfig};
use store::Store;
use wire::Event;

/// Runs the dataflow at `path` until every node has exited, deploying it
/// first if it has `build:` nodes. Returns whether all nodes succeeded.
pub fn run(path: &Path) -> io::Result<bool> {
    let dataflow = Dataflow::load(path)?;
    dataflow.resolve()?;
    if dataflow.nodes.iter().any(|n| n.build.is_some()) {
        return start(&packaging::deploy(path)?);
    }
    let name = std::fs::canonicalize(path)?;
    let base_dir = name.parent().unwrap().to_owned();
    run_dataflow(name, dataflow, base_dir, BTreeMap::new(), None)
}

/// Runs a deployment: built nodes run their binary from the store.
pub fn start(deployment: &Deployment) -> io::Result<bool> {
    let (name, dataflow) = (deployment.source.clone(), deployment.dataflow.clone());
    let id = Some(deployment.id.clone());
    run_dataflow(name, dataflow, deployment.base_dir.clone(), deployment.binaries.clone(), id)
}

/// Runs a dataflow on this machine only, with `build:` nodes built for this
/// machine and not recorded as a deployment: what `keel replay` runs.
pub fn run_here(name: PathBuf, mut dataflow: Dataflow, base_dir: PathBuf) -> io::Result<bool> {
    dataflow.machines.clear();
    dataflow.nodes.iter_mut().for_each(|n| n.machine = None);
    let executables = packaging::build_here(&dataflow, &base_dir)?;
    run_local(name, dataflow, base_dir, executables, None)
}

fn run_dataflow(
    name: PathBuf,
    dataflow: Dataflow,
    base_dir: PathBuf,
    binaries: BTreeMap<String, String>,
    deployment: Option<String>,
) -> io::Result<bool> {
    if dataflow.machines.is_empty() {
        let store = Store::open()?;
        let executables =
            (binaries.iter()).map(|(node, hash)| Ok((node.clone(), store.blob(hash)?))).collect::<io::Result<_>>()?;
        run_local(name, dataflow, base_dir, executables, deployment)
    } else {
        coordinator::run(name, dataflow, base_dir, binaries, deployment)
    }
}

/// Plays both roles in one process: coordinator and the only daemon.
fn run_local(
    name: PathBuf,
    dataflow: Dataflow,
    base_dir: PathBuf,
    executables: BTreeMap<String, PathBuf>,
    deployment: Option<String>,
) -> io::Result<bool> {
    let runtime = RuntimeDir::create()?;
    let control_listener = UnixListener::bind(runtime.control_socket())?;
    signals::install();

    let (events_tx, events) = mpsc::channel();
    let config = SessionConfig { name, machine: None, dataflow, base_dir, executables, deployment };
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
