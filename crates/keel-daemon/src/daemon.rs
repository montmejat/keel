//! `keel daemon`: a long-lived service that runs its machine's share of the
//! dataflows a coordinator sends it, one at a time.
//!
//! It listens on TCP for coordinators and for data from other daemons (see
//! `wire`). Nothing is authenticated: anyone who can reach the port can run
//! programs on this machine, so it listens on localhost unless told
//! otherwise.

use std::io::{self, BufReader, Read};
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use crate::control::{self, Reply, Request, Status};
use crate::runtime::{RuntimeDir, SessionFiles};
use crate::session::{Session, SessionConfig};
use crate::signals;
use crate::wire::{self, Event, ToDaemon};

struct Daemon {
    runtime_dir: PathBuf,
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

    let daemon = Arc::new(Daemon { runtime_dir: runtime.dir.clone(), session: Mutex::new(None) });
    {
        let daemon = daemon.clone();
        control::serve(control_listener, move |request| match daemon.current() {
            Some(session) => session.handle(request),
            None => idle_reply(request),
        });
    }
    eprintln!(
        "[daemon] pid {}, listening on {}, control socket {}",
        std::process::id(),
        listener.local_addr()?,
        runtime.control_socket().display()
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
        }),
        Request::Logs { .. } => Reply::Logs(control::Logs { lines: Vec::new(), next: 0 }),
        Request::Stop => Reply::Error("no dataflow is running".into()),
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
            _ => {}
        }
    }

    /// Runs the dataflow a coordinator sends, relaying events both ways.
    fn serve_coordinator(self: &Arc<Self>, stream: TcpStream) -> io::Result<()> {
        let writer = Arc::new(Mutex::new(stream.try_clone()?));
        let send = |event: &Event| wire::write_json(&mut *writer.lock().unwrap(), event);
        let mut reader = BufReader::new(stream);
        let Some(ToDaemon::Spawn { name, machine, dataflow, base_dir }) = wire::read_json(&mut reader)? else {
            return send(&Event::Error { message: "expected `spawn` first".into() });
        };

        let (events_tx, events) = mpsc::channel();
        let session = {
            let mut current = self.session.lock().unwrap();
            if current.is_some() {
                return send(&Event::Error { message: "this daemon is already running a dataflow".into() });
            }
            let config = SessionConfig { name: name.clone(), machine: Some(machine.clone()), dataflow, base_dir };
            let launched =
                SessionFiles::create(&self.runtime_dir).and_then(|files| Session::launch(config, files, events_tx));
            match launched {
                Ok(session) => current.insert(session).clone(),
                Err(e) => return send(&Event::Error { message: e.to_string() }),
            }
        };
        eprintln!("[daemon] running {} as machine `{machine}`", name.display());

        {
            let (daemon, writer) = (self.clone(), writer.clone());
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
        }

        loop {
            match wire::read_json(&mut reader) {
                Ok(Some(ToDaemon::Start)) => {
                    if let Err(e) = session.start() {
                        send(&Event::Error { message: e.to_string() })?;
                        session.abort();
                    }
                }
                Ok(Some(ToDaemon::Stop)) => session.stop(),
                Ok(Some(ToDaemon::Abort)) => session.abort(),
                Ok(Some(ToDaemon::Spawn { .. })) => {
                    send(&Event::Error { message: "already running a dataflow".into() })?;
                }
                Ok(None) | Err(_) => {
                    if !session.is_closed() {
                        session.log("daemon", "lost the coordinator, aborting".into());
                        session.abort();
                    }
                    return Ok(());
                }
            }
        }
    }
}
