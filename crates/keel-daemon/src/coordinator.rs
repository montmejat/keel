//! The coordinator: `keel run` for a dataflow spread over machines.
//!
//! Sends each machine's daemon the dataflow, lets every node start once all of
//! them have registered, and relays stop requests so that all machines stop
//! together. It isn't on the data path: daemons send data to each other.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::Path;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use crate::dataflow::Dataflow;
use crate::signals;
use crate::wire::{self, Event, ToDaemon};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) fn run(path: &Path, dataflow: Dataflow) -> io::Result<bool> {
    let name = std::fs::canonicalize(path)?;
    let base_dir = name.parent().unwrap().to_owned();
    signals::install();

    // Events from every daemon, tagged with its machine; `None` when its
    // connection closes.
    let (events_tx, events) = mpsc::channel::<(String, Option<Event>)>();
    let mut daemons: BTreeMap<String, TcpStream> = BTreeMap::new();
    for (machine, address) in &dataflow.machines {
        let spawn = ToDaemon::Spawn {
            name: name.clone(),
            machine: machine.clone(),
            dataflow: dataflow.clone(),
            base_dir: base_dir.clone(),
        };
        let stream = connect(address).and_then(|mut stream| {
            stream.write_all(&[wire::COORDINATOR])?;
            wire::write_json(&mut stream, &spawn)?;
            Ok(stream)
        });
        let stream = match stream {
            Ok(stream) => stream,
            Err(e) => {
                broadcast(&mut daemons, &ToDaemon::Abort);
                return Err(io::Error::other(format!(
                    "can't reach the daemon of machine `{machine}` at {address}: {e}"
                )));
            }
        };
        let (mut reader, events_tx, machine_name) =
            (BufReader::new(stream.try_clone()?), events_tx.clone(), machine.clone());
        std::thread::spawn(move || {
            while let Ok(Some(event)) = wire::read_json(&mut reader) {
                let _ = events_tx.send((machine_name.clone(), Some(event)));
            }
            let _ = events_tx.send((machine_name, None));
        });
        daemons.insert(machine.clone(), stream);
    }
    say(format!("running {} nodes on {} machines", dataflow.nodes.len(), daemons.len()));

    let (mut registered, mut finished) = (BTreeSet::new(), BTreeSet::new());
    let (mut ok, mut stopping, mut aborting, mut signals_seen) = (true, false, false, 0);
    let mut abort = |daemons: &mut BTreeMap<String, TcpStream>, reason: String| {
        if !std::mem::replace(&mut aborting, true) {
            say(format!("{reason}, aborting the dataflow"));
            broadcast(daemons, &ToDaemon::Abort);
        }
    };
    while finished.len() < daemons.len() {
        match events.recv_timeout(Duration::from_millis(50)) {
            Ok((machine, Some(event))) => match event {
                Event::AllRegistered => {
                    registered.insert(machine);
                    if registered.len() == daemons.len() {
                        say("all nodes registered on all machines, starting".into());
                        broadcast(&mut daemons, &ToDaemon::Start);
                    }
                }
                Event::Log { line } => println!("[{}@{machine}] {}", line.node, line.text),
                Event::NodeExited { node, success: false } if !stopping => {
                    ok = false;
                    abort(&mut daemons, format!("`{node}` failed on `{machine}`"));
                }
                Event::NodeExited { success, .. } => ok &= success,
                Event::StopRequested if !stopping => {
                    stopping = true;
                    say(format!("stop requested on `{machine}`, stopping all machines"));
                    broadcast(&mut daemons, &ToDaemon::Stop);
                }
                Event::StopRequested => {}
                Event::Finished { ok: machine_ok } => {
                    ok &= machine_ok;
                    finished.insert(machine);
                }
                Event::Error { message } => {
                    ok = false;
                    // The daemon won't run its share: don't wait for it.
                    finished.insert(machine.clone());
                    abort(&mut daemons, format!("`{machine}`: {message}"));
                }
            },
            Ok((machine, None)) => {
                if finished.insert(machine.clone()) {
                    ok = false;
                    abort(&mut daemons, format!("lost the connection to `{machine}`"));
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        let signals = signals::received();
        if signals != signals_seen {
            signals_seen = signals;
            if signals == 1 && !stopping {
                stopping = true;
                say("stopping all machines".into());
                broadcast(&mut daemons, &ToDaemon::Stop);
            } else if signals > 1 {
                ok = false;
                abort(&mut daemons, "interrupted again".into());
            }
        }
    }
    say(format!("dataflow finished{}", if ok { "" } else { " with failures" }));
    Ok(ok)
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

fn broadcast(daemons: &mut BTreeMap<String, TcpStream>, message: &ToDaemon) {
    for stream in daemons.values_mut() {
        // A daemon we can't reach shows up as a closed connection.
        let _ = wire::write_json(stream, message);
    }
}

fn say(text: String) {
    eprintln!("[coordinator] {text}");
}
