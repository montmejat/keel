//! A session: this machine's share of one running dataflow.
//!
//! Spawns the local nodes, waits for all of them to register, then routes each
//! output to the inputs subscribed to it. A node gets `Stop` once all of its
//! upstream nodes have exited, wherever they run.
//!
//! Payloads never pass through the session for local targets: nodes exchange
//! shared-memory regions, and the session forwards descriptors and keeps the
//! regions' reference counts right (see `keel::shm`). For targets on another
//! machine, it copies the payload onto a TCP connection to that machine's
//! daemon, which writes it into a local region of its own named after the
//! source node, so receivers can't tell the difference.
//!
//! The session acts as a small init for its nodes: it captures their output,
//! stops them in dataflow order (`Stop`, then SIGTERM, then SIGKILL), and
//! makes sure they die with it. What happens is reported as [`Event`]s, to
//! `keel run` or to the coordinator.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::mpsc::Sender;
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use keel::protocol::{DaemonMsg, NodeMsg, PeerMsg, ENV_DAEMON_SOCKET, ENV_NODE_ID, ENV_SHM_DIR};
use keel::shm::{self, Pool, Region};
use keel::trace::{self, ENV_NODE_INDEX};

use crate::control::{self, Clock, LinkStatus, LogLine, NodeState, NodeStatus, Reply, Request, Status};
use crate::dataflow::{Dataflow, NodeConfig, Routes};
use crate::runtime::SessionFiles;
use crate::tracing::Tracing;
use crate::wire::{self, Event};

/// After `Stop`, how long a node gets to exit before SIGTERM, then SIGKILL.
const STOP_GRACE: Duration = Duration::from_secs(2);
const TERM_GRACE: Duration = Duration::from_secs(3);
/// While stopping, how long the dataflow may go without progress (a message
/// delivered, a node gone) before every node gets `Stop`. Only a safety net:
/// on a slow link, draining takes as long as the queued data does.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
const LOG_CAPACITY: usize = 10_000;
/// How often the nodes' trace events are collected.
const TRACE_COLLECT_INTERVAL: Duration = Duration::from_millis(20);

/// Spawned nodes, and the threads capturing their output.
type Spawned = (Vec<(String, Child)>, Vec<JoinHandle<()>>);

pub(crate) struct SessionConfig {
    /// The dataflow file, for display.
    pub name: PathBuf,
    /// `None` when the whole dataflow runs here.
    pub machine: Option<String>,
    pub dataflow: Dataflow,
    /// Node paths are relative to this.
    pub base_dir: PathBuf,
}

pub(crate) struct Session {
    state: Mutex<State>,
    routes: Routes,
    /// Nodes on a cycle, stopped along with the sources.
    cyclic: HashSet<String>,
    /// Position of every node in the dataflow, part of its span ids.
    node_index: HashMap<String, u16>,
    tracing: Tracing,
    machine: Option<String>,
    /// Node -> machine; empty when everything is local.
    machine_of: HashMap<String, String>,
    /// Machine -> daemon address.
    addresses: BTreeMap<String, String>,
    /// Machines hosting targets of each local `(node, output)`.
    remote_targets: HashMap<(String, String), Vec<String>>,
    /// Machines hosting nodes downstream of each local node.
    downstream_machines: HashMap<String, Vec<String>>,
    /// Outgoing data connections, by machine; opened by `start`.
    peers: Mutex<HashMap<String, Arc<Mutex<TcpStream>>>>,
    shm_dir: PathBuf,
    files: Mutex<Option<SessionFiles>>,
    events: Sender<Event>,
}

struct State {
    name: PathBuf,
    start: Instant,
    expected: HashSet<String>,
    writers: HashMap<String, Arc<Mutex<UnixStream>>>,
    /// Nodes still running upstream of each local node.
    upstream: HashMap<String, HashSet<String>>,
    nodes: BTreeMap<String, NodeInfo>,
    /// Messages and bytes delivered, by `(node, output)`.
    counters: BTreeMap<(String, String), (u64, u64)>,
    logs: VecDeque<LogLine>,
    next_log: u64,
    /// Log lines also go to the coordinator.
    forward_logs: Option<Sender<Event>>,
    /// Last time a message was delivered or a node went away.
    last_progress: Instant,
    stop_requested: bool,
    stopping: bool,
    aborted: bool,
    closed: bool,
}

struct NodeInfo {
    pid: Option<u32>,
    state: NodeState,
    /// The node's regions, as mapped by its connection handler.
    regions: Vec<Arc<Region>>,
    stop_sent: Option<Instant>,
    /// We sent it SIGTERM, so dying of it counts as a clean exit.
    terminated: bool,
}

impl State {
    fn log(&mut self, node: &str, text: String) {
        eprintln!("[{node}] {text}");
        if self.logs.len() == LOG_CAPACITY {
            self.logs.pop_front();
        }
        let t_ms = self.start.elapsed().as_millis() as u64;
        let line = LogLine { seq: self.next_log, t_ms, node: node.to_owned(), text };
        if let Some(events) = &self.forward_logs {
            let _ = events.send(Event::Log { line: line.clone() });
        }
        self.logs.push_back(line);
        self.next_log += 1;
    }

    fn set_state(&mut self, node: &str, state: NodeState) {
        if let Some(info) = self.nodes.get_mut(node) {
            info.state = state;
        }
    }
}

impl Session {
    /// Spawns this machine's nodes. They register, then wait for [`Session::start`].
    pub fn launch(config: SessionConfig, files: SessionFiles, events: Sender<Event>) -> io::Result<Arc<Self>> {
        let graph = config.dataflow.resolve()?;
        let local_nodes: Vec<NodeConfig> =
            config.dataflow.nodes.iter().filter(|n| n.machine == config.machine).cloned().collect();
        let is_local = |id: &str| local_nodes.iter().any(|n| n.id == id);
        let machine_of: HashMap<String, String> =
            (config.dataflow.nodes.iter()).filter_map(|n| Some((n.id.clone(), n.machine.clone()?))).collect();

        let mut remote_targets: HashMap<(String, String), Vec<String>> = HashMap::new();
        let mut downstream_machines: HashMap<String, Vec<String>> = HashMap::new();
        for ((source, output), targets) in &graph.routes {
            if !is_local(source) {
                continue;
            }
            for (target, _) in targets.iter().filter(|(t, _)| !is_local(t)) {
                let machine = &machine_of[target];
                for list in [
                    remote_targets.entry((source.clone(), output.clone())).or_default(),
                    downstream_machines.entry(source.clone()).or_default(),
                ] {
                    if !list.contains(machine) {
                        list.push(machine.clone());
                    }
                }
            }
        }

        let node_index = (config.dataflow.nodes.iter().enumerate()).map(|(i, n)| (n.id.clone(), i as u16)).collect();
        let feeds = (graph.routes.iter())
            .flat_map(|(from, targets)| targets.iter().map(move |to| (to.clone(), from.clone())))
            .filter(|((node, _), _)| is_local(node))
            .collect();
        let tracing = Tracing::new(
            files.shm_dir.clone(),
            config.machine.clone(),
            machine_of.clone(),
            feeds,
            local_nodes.iter().map(|n| n.id.clone()),
        );

        let node_listener = UnixListener::bind(&files.nodes_socket)?;
        let session = Arc::new(Session {
            state: Mutex::new(State {
                name: config.name,
                start: Instant::now(),
                expected: local_nodes.iter().map(|n| n.id.clone()).collect(),
                writers: HashMap::new(),
                upstream: (graph.upstream.into_iter()).filter(|(id, _)| is_local(id)).collect(),
                nodes: (local_nodes.iter())
                    .map(|n| {
                        let info = NodeInfo {
                            pid: None,
                            state: NodeState::Starting,
                            regions: Vec::new(),
                            stop_sent: None,
                            terminated: false,
                        };
                        (n.id.clone(), info)
                    })
                    .collect(),
                counters: (graph.routes.keys()).filter(|(n, _)| is_local(n)).map(|k| (k.clone(), (0, 0))).collect(),
                logs: VecDeque::new(),
                next_log: 0,
                forward_logs: config.machine.is_some().then(|| events.clone()),
                last_progress: Instant::now(),
                stop_requested: false,
                stopping: false,
                aborted: false,
                closed: false,
            }),
            routes: graph.routes,
            cyclic: graph.cyclic,
            node_index,
            tracing,
            machine: config.machine,
            machine_of,
            addresses: config.dataflow.machines,
            remote_targets,
            downstream_machines,
            peers: Mutex::new(HashMap::new()),
            shm_dir: files.shm_dir.clone(),
            files: Mutex::new(Some(files)),
            events,
        });

        {
            let session = session.clone();
            std::thread::spawn(move || {
                for stream in node_listener.incoming().flatten() {
                    if session.state.lock().unwrap().closed {
                        break;
                    }
                    let session = session.clone();
                    std::thread::spawn(move || session.handle_node(stream));
                }
            });
        }

        {
            let session = session.clone();
            std::thread::spawn(move || {
                while !session.is_closed() {
                    session.tracing.collect();
                    std::thread::sleep(TRACE_COLLECT_INTERVAL);
                }
            });
        }

        // Nodes are spawned from the supervising thread: their parent-death
        // signal fires when the thread that spawned them exits.
        let (spawned_tx, spawned_rx) = mpsc::channel();
        {
            let session = session.clone();
            std::thread::spawn(move || {
                let (children, output_threads) = match session.spawn_all(&local_nodes, &config.base_dir) {
                    Ok(spawned) => spawned,
                    Err(e) => {
                        session.cleanup();
                        let _ = spawned_tx.send(Err(e));
                        return;
                    }
                };
                let _ = spawned_tx.send(Ok(()));
                if local_nodes.is_empty() {
                    session.emit(Event::AllRegistered);
                }
                let ok = session.supervise(children);
                for thread in output_threads {
                    let _ = thread.join();
                }
                session.cleanup();
                session.emit(Event::Finished { ok });
            });
        }
        spawned_rx.recv().map_err(io::Error::other)??;
        Ok(session)
    }

    /// Lets the nodes run, once every node of the dataflow has registered.
    pub fn start(&self) -> io::Result<()> {
        let mut machines: Vec<&String> = self.downstream_machines.values().flatten().collect();
        machines.sort();
        machines.dedup();
        for machine in machines {
            let address = &self.addresses[machine];
            let mut stream = TcpStream::connect(address).map_err(|e| {
                io::Error::other(format!("can't reach the daemon of machine `{machine}` at {address}: {e}"))
            })?;
            stream.set_nodelay(true)?;
            stream.write_all(&[wire::PEER])?;
            self.peers.lock().unwrap().insert(machine.clone(), Arc::new(Mutex::new(stream)));
        }
        let mut s = self.state.lock().unwrap();
        let ids: Vec<String> = s.writers.keys().cloned().collect();
        for id in ids {
            let _ = DaemonMsg::Ready.write_to(&mut *s.writers[&id].lock().unwrap());
            s.set_state(&id, NodeState::Running);
        }
        Ok(())
    }

    /// Someone wants the dataflow stopped. The session reports it and waits
    /// for [`Session::stop`], so that every machine stops together.
    pub fn request_stop(&self) {
        let mut s = self.state.lock().unwrap();
        if !std::mem::replace(&mut s.stop_requested, true) {
            drop(s);
            self.emit(Event::StopRequested);
        }
    }

    pub fn stop(&self) {
        self.state.lock().unwrap().stopping = true;
    }

    pub fn abort(&self) {
        self.state.lock().unwrap().aborted = true;
    }

    pub fn is_closed(&self) -> bool {
        self.state.lock().unwrap().closed
    }

    pub fn log(&self, node: &str, text: String) {
        self.state.lock().unwrap().log(node, text);
    }

    /// Clock offsets measured by the coordinator.
    pub fn set_clocks(&self, clocks: BTreeMap<String, Clock>) {
        self.tracing.set_clocks(clocks);
    }

    fn emit(&self, event: Event) {
        let _ = self.events.send(event);
    }

    pub fn handle(&self, request: Request) -> Reply {
        match request {
            Request::Status => Reply::Status(self.status()),
            Request::Logs { since } => {
                let s = self.state.lock().unwrap();
                let lines = s.logs.iter().filter(|l| l.seq >= since).cloned().collect();
                Reply::Logs(control::Logs { lines, next: s.next_log })
            }
            Request::Stop => {
                self.request_stop();
                Reply::Stopping
            }
            Request::Trace => Reply::Trace(self.tracing.report()),
        }
    }

    fn status(&self) -> Status {
        let s = self.state.lock().unwrap();
        let nodes = (s.nodes.iter())
            .map(|(id, info)| NodeStatus {
                id: id.clone(),
                machine: self.machine.clone(),
                pid: info.pid,
                state: info.state.clone(),
                shm_regions: info.regions.len() as u32,
                shm_held: info.regions.iter().filter(|r| r.refcount().load(Ordering::Relaxed) > 0).count() as u32,
                shm_bytes: info.regions.iter().map(|r| r.capacity() as u64).sum(),
            })
            .collect();
        let endpoint = |node: &str, port: &str| match self.machine_of.get(node) {
            Some(machine) if Some(machine) != self.machine.as_ref() => format!("{node}/{port}@{machine}"),
            _ => format!("{node}/{port}"),
        };
        let links = (s.counters.iter())
            .map(|((node, output), &(messages, bytes))| LinkStatus {
                source: endpoint(node, output),
                targets: (self.routes.get(&(node.clone(), output.clone())).into_iter().flatten())
                    .map(|(n, i)| endpoint(n, i))
                    .collect(),
                messages,
                bytes,
            })
            .collect();
        Status {
            pid: std::process::id(),
            machine: self.machine.clone(),
            dataflow: Some(s.name.clone()),
            uptime_ms: s.start.elapsed().as_millis() as u64,
            stopping: s.stop_requested || s.stopping,
            nodes,
            links,
            coordinator: false,
        }
    }

    /// Starts each node in its own process group, with its output captured.
    fn spawn_all(self: &Arc<Self>, nodes: &[NodeConfig], base_dir: &Path) -> io::Result<Spawned> {
        let (nodes_socket, shm_dir) = {
            let files = self.files.lock().unwrap();
            let files = files.as_ref().unwrap();
            (files.nodes_socket.clone(), files.shm_dir.clone())
        };
        let mut children: Vec<(String, Child)> = Vec::new();
        let mut output_threads = Vec::new();
        for node in nodes {
            let exe = base_dir.join(&node.path);
            let mut command = Command::new(&exe);
            command
                .env(ENV_NODE_ID, &node.id)
                .env(ENV_NODE_INDEX, self.node_index[&node.id].to_string())
                .env(ENV_DAEMON_SOCKET, &nodes_socket)
                .env(ENV_SHM_DIR, &shm_dir)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                // Keeps Ctrl-C in the terminal from reaching nodes directly:
                // the session decides how they stop.
                .process_group(0);
            // SAFETY: prctl is async-signal-safe.
            unsafe {
                command.pre_exec(|| match libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) {
                    -1 => Err(io::Error::last_os_error()),
                    _ => Ok(()),
                });
            }
            let mut child = match command.spawn() {
                Ok(child) => child,
                Err(e) => {
                    self.kill_all(&mut children);
                    return Err(io::Error::other(format!("failed to spawn `{}` ({}): {e}", node.id, exe.display())));
                }
            };
            self.state.lock().unwrap().nodes.get_mut(&node.id).unwrap().pid = Some(child.id());

            let capture = |pipe: Box<dyn Read + Send>| {
                let (session, id) = (self.clone(), node.id.clone());
                std::thread::spawn(move || {
                    for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                        session.log(&id, line);
                    }
                })
            };
            output_threads.push(capture(Box::new(child.stdout.take().unwrap())));
            output_threads.push(capture(Box::new(child.stderr.take().unwrap())));
            children.push((node.id.clone(), child));
        }
        Ok((children, output_threads))
    }

    fn handle_node(&self, mut stream: UnixStream) {
        let node_id = match NodeMsg::read_from(&mut stream) {
            Ok(Some(NodeMsg::Register { node_id })) => node_id,
            // The wake-up connection from `cleanup`, or a stray client.
            _ => return,
        };
        {
            let mut s = self.state.lock().unwrap();
            if !s.expected.contains(&node_id) || s.writers.contains_key(&node_id) {
                s.log("daemon", format!("rejecting unknown or duplicate node `{node_id}`"));
                return;
            }
            let Ok(writer) = stream.try_clone() else { return };
            s.writers.insert(node_id.clone(), Arc::new(Mutex::new(writer)));
            if s.writers.len() == s.expected.len() {
                let count = s.expected.len();
                s.log("daemon", format!("all {count} local nodes registered"));
                drop(s);
                self.emit(Event::AllRegistered);
            }
        }

        // This node's regions, mapped to adjust their reference counts and to
        // read payloads bound for other machines.
        let mut regions: HashMap<u32, Arc<Region>> = HashMap::new();
        loop {
            match NodeMsg::read_from(&mut stream) {
                Ok(Some(NodeMsg::Output { output_id, slot, len })) => {
                    let region = match regions.get(&slot) {
                        Some(region) if region.capacity() >= len as usize => region.clone(),
                        _ => match Region::open(&shm::region_path(&self.shm_dir, &node_id, slot)) {
                            Ok(region) => {
                                let region = Arc::new(region);
                                regions.insert(slot, region.clone());
                                let mut s = self.state.lock().unwrap();
                                let node_regions = &mut s.nodes.get_mut(&node_id).unwrap().regions;
                                match node_regions.get_mut(slot as usize) {
                                    Some(existing) => *existing = region.clone(),
                                    None => node_regions.push(region.clone()),
                                }
                                region
                            }
                            // The session is over and removed its files.
                            Err(_) if self.is_closed() => break,
                            Err(err) => {
                                self.log(
                                    "daemon",
                                    format!("`{node_id}` sent from region {slot}, which can't be opened: {err}"),
                                );
                                break;
                            }
                        },
                    };
                    self.deliver(&node_id, &output_id, slot, len, &region, true);
                }
                Ok(None) => break,
                Ok(Some(other)) => {
                    self.log("daemon", format!("unexpected message from `{node_id}`: {other:?}"));
                    break;
                }
                // The node died; `supervise` reports how.
                Err(e) if e.kind() == io::ErrorKind::ConnectionReset => break,
                Err(e) => {
                    self.log("daemon", format!("connection to `{node_id}` failed: {e}"));
                    break;
                }
            }
        }
        self.node_disconnected(&node_id);
    }

    /// Receives what a daemon on another machine forwards to our nodes.
    pub fn serve_peer(&self, stream: TcpStream) {
        let mut reader = BufReader::with_capacity(1 << 20, stream);
        // Regions standing in for each remote source, named after it.
        let mut pools: HashMap<String, Pool> = HashMap::new();
        loop {
            match PeerMsg::read_from(&mut reader) {
                Ok(Some(PeerMsg::Data { source, output, mut context, payload })) => {
                    let received = trace::now_ns();
                    context.published_ns = self.tracing.to_local(context.published_ns, &source);
                    let pool =
                        pools.entry(source.clone()).or_insert_with(|| Pool::new(self.shm_dir.clone(), source.clone()));
                    let slot = match pool.acquire(payload.len()) {
                        Ok(slot) => slot,
                        Err(e) => {
                            self.log("daemon", format!("dropping a message from `{source}`: {e}"));
                            continue;
                        }
                    };
                    pool.payload_mut(slot, payload.len()).copy_from_slice(&payload);
                    pool.region(slot).set_context(&context);
                    pool.publish(slot);
                    if context.sampled {
                        self.tracing.net_received(&context, &source, &output, received);
                    }
                    self.deliver(&source, &output, slot, payload.len() as u64, pool.region(slot), false);
                }
                Ok(Some(PeerMsg::Closed { node })) => self.node_disconnected(&node),
                Ok(None) => break,
                Err(e) => {
                    self.log("daemon", format!("data connection from another machine failed: {e}"));
                    break;
                }
            }
        }
    }

    /// Hands a message in `region` to the local nodes subscribed to
    /// `source/output`, and with `forward`, to other machines. Consumes the
    /// in-transit reference.
    fn deliver(&self, source: &str, output: &str, slot: u32, len: u64, region: &Region, forward: bool) {
        let context = region.context();
        if context.sampled && forward {
            self.tracing.routed(&context, source, output, trace::now_ns());
        }
        let key = (source.to_owned(), output.to_owned());
        let writers: Vec<_> = {
            let mut s = self.state.lock().unwrap();
            s.last_progress = Instant::now();
            let counter = s.counters.entry(key.clone()).or_default();
            counter.0 += 1;
            counter.1 += len;
            (self.routes.get(&key).into_iter().flatten())
                .filter_map(|(target, input_id)| Some((s.writers.get(target)?.clone(), target, input_id)))
                .collect()
        };
        if let Some(machines) = self.remote_targets.get(&key).filter(|_| forward) {
            // SAFETY: the in-transit reference keeps the payload stable.
            let payload = unsafe { region.payload_slice(len as usize) };
            for machine in machines {
                let Some(peer) = self.peers.lock().unwrap().get(machine).cloned() else { continue };
                let written = PeerMsg::write_data(&mut *peer.lock().unwrap(), source, output, &context, payload);
                if let Err(e) = written {
                    self.peers.lock().unwrap().remove(machine);
                    self.log("daemon", format!("lost the data connection to machine `{machine}`: {e}"));
                } else if context.sampled {
                    self.tracing.net_sent(&context, machine, trace::now_ns());
                }
            }
        }
        for (writer, target, input_id) in writers {
            // Take the receiver's reference before it can see the message.
            region.refcount().fetch_add(1, Ordering::Relaxed);
            if context.sampled {
                self.tracing.delivered(&context, target, input_id, trace::now_ns());
            }
            let msg = DaemonMsg::Input { input_id: input_id.clone(), source: source.to_owned(), slot, len };
            if msg.write_to(&mut *writer.lock().unwrap()).is_err() {
                // The target has exited; it will never release it.
                region.refcount().fetch_sub(1, Ordering::Release);
            }
        }
        // Drop the in-transit reference.
        region.refcount().fetch_sub(1, Ordering::Release);
    }

    /// `node_id` (local, or on another machine) is gone: sends `Stop` to local
    /// nodes that have no upstream left, and tells machines downstream.
    fn node_disconnected(&self, node_id: &str) {
        {
            let mut s = self.state.lock().unwrap();
            s.last_progress = Instant::now();
            s.writers.remove(node_id);
            let mut to_stop = Vec::new();
            for (id, sources) in s.upstream.iter_mut() {
                if sources.remove(node_id) && sources.is_empty() {
                    to_stop.push(id.clone());
                }
            }
            for id in to_stop {
                send_stop(&mut s, &id);
            }
        }
        // Same connection as the node's data, so this arrives after all of it.
        for machine in self.downstream_machines.get(node_id).into_iter().flatten() {
            let Some(peer) = self.peers.lock().unwrap().get(machine).cloned() else { continue };
            let _ = PeerMsg::Closed { node: node_id.to_owned() }.write_to(&mut *peer.lock().unwrap());
        }
    }

    /// Waits for all local nodes to exit. If one fails, kills the others.
    ///
    /// Stopping drains the dataflow: sources and nodes on cycles get `Stop`
    /// first, and each other node gets it once all its upstream nodes have
    /// exited, so no in-flight message is dropped. If that stalls for
    /// `DRAIN_TIMEOUT`, every node gets it. A node that ignores `Stop` gets
    /// SIGTERM, then SIGKILL.
    fn supervise(&self, mut children: Vec<(String, Child)>) -> bool {
        let mut ok = true;
        let mut stop_started: Option<Instant> = None;
        let mut drained = false;
        while !children.is_empty() {
            let mut i = 0;
            while i < children.len() {
                match children[i].1.try_wait() {
                    Ok(Some(status)) => {
                        let (id, _) = children.remove(i);
                        let mut s = self.state.lock().unwrap();
                        s.last_progress = Instant::now();
                        // A node we terminated did what it was asked.
                        let terminated = s.nodes[&id].terminated && status.signal() == Some(libc::SIGTERM);
                        let success = status.success() || terminated;
                        s.log("daemon", format!("`{id}` exited: {status}"));
                        s.set_state(&id, NodeState::Exited { success, detail: status.to_string() });
                        let stopping = s.stopping;
                        drop(s);
                        self.emit(Event::NodeExited { node: id.clone(), success });
                        if !success {
                            ok = false;
                            if !stopping {
                                self.log("daemon", format!("stopping the dataflow because `{id}` failed"));
                                self.kill_all(&mut children);
                            }
                        }
                    }
                    Ok(None) => i += 1,
                    Err(e) => {
                        self.log("daemon", format!("failed to wait for `{}`: {e}", children[i].0));
                        children.remove(i);
                        ok = false;
                    }
                }
            }

            if self.state.lock().unwrap().aborted && !children.is_empty() {
                self.log("daemon", "aborting, killing all nodes".into());
                self.kill_all(&mut children);
                ok = false;
            }
            let mut s = self.state.lock().unwrap();
            if s.stopping && stop_started.is_none() {
                stop_started = Some(Instant::now());
                s.last_progress = Instant::now();
                s.log("daemon", "stopping the dataflow, starting with its sources".into());
                let sources: Vec<String> = (s.upstream.iter())
                    .filter(|(id, up)| up.is_empty() || self.cyclic.contains(*id))
                    .map(|(id, _)| id.clone())
                    .collect();
                for id in sources {
                    send_stop(&mut s, &id);
                }
            }
            if !drained && stop_started.is_some() && s.last_progress.elapsed() > DRAIN_TIMEOUT {
                drained = true;
                let ids: Vec<String> = s.nodes.keys().cloned().collect();
                let stuck =
                    |info: &NodeInfo| info.stop_sent.is_none() && !matches!(info.state, NodeState::Exited { .. });
                if ids.iter().any(|id| stuck(&s.nodes[id])) {
                    s.log(
                        "daemon",
                        format!("dataflow stalled for {DRAIN_TIMEOUT:?} while stopping, stopping the remaining nodes"),
                    );
                }
                for id in ids {
                    send_stop(&mut s, &id);
                }
            }
            for (id, child) in &mut children {
                let info = s.nodes.get_mut(id).unwrap();
                let Some(elapsed) = info.stop_sent.map(|t| t.elapsed()) else { continue };
                if elapsed > STOP_GRACE + TERM_GRACE {
                    let _ = child.kill();
                    s.log("daemon", format!("`{id}` ignored SIGTERM, killing it"));
                } else if elapsed > STOP_GRACE && !info.terminated {
                    info.terminated = true;
                    // SAFETY: plain syscall on a child we haven't reaped yet.
                    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
                    s.log("daemon", format!("`{id}` still running {STOP_GRACE:?} after Stop, sending SIGTERM"));
                }
            }
            drop(s);
            std::thread::sleep(Duration::from_millis(20));
        }
        ok
    }

    fn kill_all(&self, children: &mut Vec<(String, Child)>) {
        for (id, child) in children.iter_mut() {
            let _ = child.kill();
            let _ = child.wait();
            let mut s = self.state.lock().unwrap();
            s.log("daemon", format!("killed `{id}`"));
            s.set_state(id, NodeState::Exited { success: false, detail: "killed".into() });
            drop(s);
            self.emit(Event::NodeExited { node: id.clone(), success: false });
        }
        children.clear();
    }

    /// Closes connections and removes the session's files.
    fn cleanup(&self) {
        self.state.lock().unwrap().closed = true;
        self.peers.lock().unwrap().clear();
        if let Some(files) = self.files.lock().unwrap().take() {
            // Wakes the accept loop so it sees `closed` and lets go of us.
            let _ = UnixStream::connect(&files.nodes_socket);
        }
    }
}

/// Asks a node to stop, once. From then on it has `STOP_GRACE` to exit before
/// `supervise` escalates.
fn send_stop(s: &mut State, node_id: &str) {
    let Some(info) = s.nodes.get_mut(node_id) else { return };
    if info.stop_sent.is_some() || matches!(info.state, NodeState::Exited { .. }) {
        return;
    }
    info.stop_sent = Some(Instant::now());
    info.state = NodeState::Stopping;
    if let Some(w) = s.writers.get(node_id) {
        let _ = DaemonMsg::Stop.write_to(&mut *w.lock().unwrap());
    }
}
