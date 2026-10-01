//! `keel daemon`: a long-lived service that runs its machine's share of the
//! dataflows a coordinator sends it, one at a time, and keeps the binaries
//! they're deployed with in its store (see `store`).
//!
//! It listens on TCP for coordinators, for data from other daemons, and for
//! binaries (see `wire`). Nothing is authenticated: anyone who can reach the
//! port can run programs on this machine, so it listens on localhost unless
//! told otherwise.

use std::collections::BTreeMap;
use std::io::{self, BufReader, Read};
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use keel::trace;

use crate::control::{self, Reply, Request, Status};
use crate::doctor;
use crate::packaging;
use crate::runtime::{RuntimeDir, SessionFiles};
use crate::session::{Session, SessionConfig};
use crate::signals;
use crate::store::Store;
use crate::wire::{self, BlobHeader, Event, ToDaemon};

struct Daemon {
    runtime_dir: PathBuf,
    /// Connections must present it, when there is one.
    token: Option<String>,
    store: Store,
    /// The dataflow being run, if any.
    session: Mutex<Option<Arc<Session>>>,
}

/// Serves until SIGINT or SIGTERM, which aborts the running dataflow.
pub fn serve(listen: &str) -> io::Result<()> {
    let listener =
        TcpListener::bind(listen).map_err(|e| io::Error::new(e.kind(), format!("can't listen on {listen}: {e}")))?;
    let runtime = RuntimeDir::create()?;
    let control_listener = UnixListener::bind(runtime.control_socket())?;
    signals::install();

    let store = Store::open()?;
    let (blobs, bytes) = store.usage();
    let token = wire::load_token();
    if token.is_none() {
        eprintln!(
            "[daemon] no token in {}: accepting any connection (`keel provision` sets one up)",
            wire::token_path().display()
        );
    }
    let daemon = Arc::new(Daemon { runtime_dir: runtime.dir.clone(), token, store, session: Mutex::new(None) });
    {
        let daemon = daemon.clone();
        control::serve(control_listener, move |request| match daemon.current() {
            Some(session) => session.handle(request),
            None => idle_reply(request),
        });
    }
    eprintln!(
        "[daemon] pid {}, listening on {}, control socket {}, store {} ({blobs} binaries, {} MiB)",
        std::process::id(),
        listener.local_addr()?,
        runtime.control_socket().display(),
        daemon.store.dir().display(),
        bytes >> 20,
    );
    {
        let daemon = daemon.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let daemon = daemon.clone();
                std::thread::spawn(move || daemon.handle_connection(stream));
            }
        });
    }

    while signals::received() == 0 {
        std::thread::sleep(Duration::from_millis(50));
    }
    eprintln!("[daemon] shutting down");
    if let Some(session) = daemon.current() {
        session.abort();
        let deadline = Instant::now() + Duration::from_secs(5);
        while daemon.current().is_some() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    Ok(())
}

fn idle_reply(request: Request) -> Reply {
    match request {
        Request::Status => Reply::Status(Status {
            pid: std::process::id(),
            machine: None,
            dataflow: None,
            uptime_ms: 0,
            stopping: false,
            nodes: Vec::new(),
            links: Vec::new(),
            coordinator: false,
            deployment: None,
        }),
        Request::Logs { .. } => Reply::Logs(control::Logs { lines: Vec::new(), next: 0 }),
        Request::Stop | Request::Trace { .. } | Request::Update { .. } => Reply::Error("no dataflow is running".into()),
    }
}

impl Daemon {
    fn current(&self) -> Option<Arc<Session>> {
        self.session.lock().unwrap().clone()
    }

    fn handle_connection(self: Arc<Self>, mut stream: TcpStream) {
        let mut kind = [0];
        if stream.read_exact(&mut kind).is_err() {
            return;
        }
        let _ = stream.set_nodelay(true);
        if let Err(e) = wire::check_token(&mut stream, self.token.as_deref()) {
            let peer = stream.peer_addr().map_or("?".into(), |a| a.to_string());
            eprintln!("[daemon] refused a connection from {peer}: {e}");
            let message = format!("{e}: this daemon's token (in its {}) isn't ours", wire::token_path().display());
            let _ = wire::write_json(&mut stream, &Event::Error { message });
            return;
        }
        match kind[0] {
            wire::COORDINATOR => {
                if let Err(e) = self.serve_coordinator(stream) {
                    eprintln!("[daemon] coordinator connection failed: {e}");
                }
            }
            wire::PEER => match self.current() {
                Some(session) => session.serve_peer(stream),
                None => eprintln!("[daemon] another machine sent data, but no dataflow is running"),
            },
            wire::BLOB => {
                if let Err(e) = self.receive_blob(stream) {
                    eprintln!("[daemon] receiving a binary failed: {e}");
                }
            }
            _ => {}
        }
    }

    /// Stores one binary, checked against its hash.
    fn receive_blob(&self, stream: TcpStream) -> io::Result<()> {
        let mut writer = stream.try_clone()?;
        let mut reader = BufReader::new(stream);
        let Some(BlobHeader { hash, len }) = wire::read_json(&mut reader)? else { return Ok(()) };
        let answer = match self.store.put(&hash, &mut reader, len) {
            Ok(()) => {
                eprintln!("[daemon] stored {hash} ({} KiB)", len >> 10);
                Event::Done
            }
            Err(e) => Event::Error { message: e.to_string() },
        };
        wire::write_json(&mut writer, &answer)
    }

    /// Answers deployment requests, and runs the dataflow a coordinator
    /// sends, relaying events both ways.
    fn serve_coordinator(self: &Arc<Self>, stream: TcpStream) -> io::Result<()> {
        let writer = Arc::new(Mutex::new(stream.try_clone()?));
        let send = |event: &Event| wire::write_json(&mut *writer.lock().unwrap(), event);
        let mut reader = BufReader::new(stream);
        let mut session: Option<Arc<Session>> = None;
        while let Ok(Some(request)) = wire::read_json(&mut reader) {
            let request = match request {
                ToDaemon::Spawn { name, machine, dataflow, base_dir, binaries, deployment } => {
                    let spawned = self.spawn(name, machine, dataflow, base_dir, (binaries, deployment), writer.clone());
                    match spawned {
                        Ok(spawned) => session = Some(spawned),
                        Err(e) => send(&Event::Error { message: e.to_string() })?,
                    }
                    continue;
                }
                ToDaemon::Hello => {
                    let (blobs, bytes) = self.store.usage();
                    send(&Event::Hello { target: packaging::host_target(), blobs, bytes })?;
                    continue;
                }
                ToDaemon::Diagnose => {
                    send(&Event::Diagnosis { checks: doctor::here() })?;
                    continue;
                }
                ToDaemon::Missing { hashes } => {
                    match self.store.missing(&hashes) {
                        Ok(hashes) => send(&Event::Missing { hashes })?,
                        Err(e) => send(&Event::Error { message: e.to_string() })?,
                    }
                    continue;
                }
                ToDaemon::Pin { id, hashes } => {
                    match self.store.pin(&id, &hashes) {
                        Ok(()) => send(&Event::Done)?,
                        Err(e) => send(&Event::Error { message: e.to_string() })?,
                    }
                    continue;
                }
                ToDaemon::Unpin { ids } => {
                    let collected = ids.iter().try_for_each(|id| self.store.unpin(id)).and_then(|()| self.store.gc());
                    match collected {
                        Ok((blobs, bytes)) => {
                            eprintln!("[daemon] gc: removed {blobs} binaries, {} KiB", bytes >> 10);
                            send(&Event::Collected { blobs, bytes })?
                        }
                        Err(e) => send(&Event::Error { message: e.to_string() })?,
                    }
                    continue;
                }
                request => request,
            };
            let Some(session) = &session else {
                send(&Event::Error { message: "no dataflow spawned on this connection".into() })?;
                continue;
            };
            match request {
                ToDaemon::Start => {
                    if let Err(e) = session.start() {
                        send(&Event::Error { message: e.to_string() })?;
                        session.abort();
                    }
                }
                ToDaemon::Stop => session.stop(),
                ToDaemon::Abort => session.abort(),
                ToDaemon::Ping { t1 } => {
                    let t2 = trace::now_ns();
                    send(&Event::Pong { t1, t2, t3: trace::now_ns() })?;
                }
                ToDaemon::Clocks { clocks } => session.set_clocks(clocks),
                ToDaemon::Control { id, request } => {
                    send(&Event::Control { id, reply: session.handle(request) })?;
                }
                _ => unreachable!("handled above"),
            }
        }
        if let Some(session) = session.filter(|s| !s.is_closed()) {
            session.log("daemon", "lost the coordinator, aborting".into());
            session.abort();
        }
        Ok(())
    }

    fn spawn(
        self: &Arc<Self>,
        name: PathBuf,
        machine: String,
        dataflow: crate::dataflow::Dataflow,
        base_dir: PathBuf,
        (binaries, deployment): (BTreeMap<String, String>, Option<String>),
        writer: Arc<Mutex<TcpStream>>,
    ) -> io::Result<Arc<Session>> {
        let mut executables = BTreeMap::new();
        for (node, hash) in binaries {
            let path = self.store.blob(&hash)?;
            if !path.exists() {
                return Err(io::Error::other(format!("`{node}`'s binary {hash} isn't in this machine's store")));
            }
            executables.insert(node, path);
        }
        let (events_tx, events) = mpsc::channel();
        let session = {
            let mut current = self.session.lock().unwrap();
            if current.is_some() {
                return Err(io::Error::other("this daemon is already running a dataflow"));
            }
            let machine = Some(machine.clone());
            let config = SessionConfig { name: name.clone(), machine, dataflow, base_dir, executables, deployment };
            let files = SessionFiles::create(&self.runtime_dir)?;
            current.insert(Session::launch(config, files, events_tx)?).clone()
        };
        eprintln!("[daemon] running {} as machine `{machine}`", name.display());
        let daemon = self.clone();
        std::thread::spawn(move || {
            for event in events {
                let finished = matches!(event, Event::Finished { .. });
                let _ = wire::write_json(&mut *writer.lock().unwrap(), &event);
                if finished {
                    *daemon.session.lock().unwrap() = None;
                    eprintln!("[daemon] dataflow finished, waiting for the next one");
                    return;
                }
            }
        });
        Ok(session)
    }
}
