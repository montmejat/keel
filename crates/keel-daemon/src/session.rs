//! A session: this machine's share of one running dataflow.
//!
//! Sets up the shared-memory files nodes talk through (a channel per input, a
//! bell per node, see `keel::channel`), spawns the local nodes, waits for all
//! of them to register, then hands each its routes. From then on local
//! messages go straight from node to node: the session isn't on that path.
//!
//! It still carries what crosses machines. Outputs with targets elsewhere
//! also feed a channel the session reads; it copies those payloads onto a TCP
//! connection to the other machine's daemon. What arrives from other
//! machines, it writes into regions of its own named after the remote source
//! and pushes into local channels, exactly as a local sender would.
//!
//! The session acts as a small init for its nodes: it captures their output,
//! applies their real-time settings, stops them in dataflow order (`Stop` in
//! their bell once all their upstream nodes have exited, then SIGTERM, then
//! SIGKILL), and makes sure they die with it. What happens is reported as
//! [`Event`]s, to `keel run` or to the coordinator.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use keel::channel::{self, Bell, Channel, Target, DAEMON};
use keel::protocol::{DaemonMsg, NodeMsg, PeerMsg, ENV_DAEMON_SOCKET, ENV_NODE_ID, ENV_REALTIME, ENV_SHM_DIR};
use keel::shm::{self, Pool, Region};
use keel::trace::{self, ENV_NODE_INDEX};

use crate::control::{self, Clock, LinkStatus, LogLine, NodeState, NodeStatus, Reply, Request, Status};
use crate::dataflow::{Dataflow, NodeConfig, Realtime, Routes};
use crate::runtime::SessionFiles;
use crate::tracing::Tracing;
use crate::wire::{self, Event};

/// After `Stop`, how long a node gets to exit before SIGTERM, then SIGKILL.
const STOP_GRACE: Duration = Duration::from_secs(2);
const TERM_GRACE: Duration = Duration::from_secs(3);
/// While stopping, how long the dataflow may go without progress (a message
/// sent, a node gone) before every node gets `Stop`. Only a safety net: on a
/// slow link, draining takes as long as the queued data does.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
const LOG_CAPACITY: usize = 10_000;
/// How often the nodes' trace events are collected.
const TRACE_COLLECT_INTERVAL: Duration = Duration::from_millis(20);

/// Spawned nodes, and the threads capturing their output.
type Spawned = (Vec<(String, Child)>, Vec<JoinHandle<()>>);
/// Local `(node, input, channel)`s fed by each remote `(node, output)`.
type Inbound = HashMap<(String, String), Vec<(String, String, Target)>>;

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
    /// What each local node is told at start: its channels and targets.
    node_routes: HashMap<String, String>,
    tracing: Tracing,
    machine: Option<String>,
    /// Node -> machine; empty when everything is local.
    machine_of: HashMap<String, String>,
    /// Machine -> daemon address.
    addresses: BTreeMap<String, String>,
    /// Machines hosting nodes downstream of each local node.
    downstream_machines: HashMap<String, Vec<String>>,
    /// Local outputs with targets on other machines, read by the forwarder.
    forwards: Vec<Forward>,
    /// Local targets of each remote `(node, output)`, fed by `serve_peer`.
    inbound: Inbound,
    /// Wakes the forwarder: rung by local senders, and on shutdown.
    daemon_bell: Bell,
    /// Local nodes that exited, for the forwarder to tell other machines,
    /// after forwarding everything they sent.
    closing: Mutex<Vec<String>>,
    /// Outgoing data connections, by machine; opened by `start`.
    peers: Mutex<HashMap<String, Arc<Mutex<TcpStream>>>>,
    /// Messages forwarded to or received from other machines: progress the
    /// nodes' own counters don't show.
    carried: AtomicU64,
    shm_dir: PathBuf,
    files: Mutex<Option<SessionFiles>>,
    events: Sender<Event>,
}

struct Forward {
    source: String,
    output: String,
    channel: Channel,
    machines: Vec<String>,
}

struct State {
    name: PathBuf,
    start: Instant,
    expected: HashSet<String>,
    /// Registered nodes' connections, to send them their routes.
    sockets: HashMap<String, UnixStream>,
    bells: HashMap<String, Bell>,
    /// Nodes still running upstream of each local node.
    upstream: HashMap<String, HashSet<String>>,
    nodes: BTreeMap<String, NodeInfo>,
    logs: VecDeque<LogLine>,
    next_log: u64,
    /// Log lines also go to the coordinator.
    forward_logs: Option<Sender<Event>>,
    stop_requested: bool,
    stopping: bool,
    aborted: bool,
    closed: bool,
}

struct NodeInfo {
    pid: Option<u32>,
    state: NodeState,
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
        let dir = &files.shm_dir;

        // Every file nodes open at start exists before any of them runs.
        let mut bells = HashMap::new();
        for node in &local_nodes {
            bells.insert(node.id.clone(), Bell::create(&channel::bell_path(dir, &node.id))?);
            for input in node.inputs.keys() {
                let keep = graph.keep[&(node.id.clone(), input.clone())];
                Channel::create(&channel::channel_path(dir, &node.id, input), keep)?;
            }
        }
        let daemon_bell = Bell::create(&channel::bell_path(dir, DAEMON))?;

        let mut node_routes: HashMap<String, String> = HashMap::new();
        for node in &local_nodes {
            let routes = node_routes.entry(node.id.clone()).or_default();
            for (input, source) in &node.inputs {
                let source_node = source.source().split('/').next().unwrap();
                routes.push_str(&format!("in {input} {source_node}\n"));
            }
        }
        let mut forwards = Vec::new();
        let mut downstream_machines: HashMap<String, Vec<String>> = HashMap::new();
        let mut inbound = Inbound::new();
        let mut sorted_routes: Vec<_> = graph.routes.iter().collect();
        sorted_routes.sort();
        for ((source, output), targets) in sorted_routes {
            if is_local(source) {
                let routes = node_routes.get_mut(source).unwrap();
                let mut machines: Vec<String> = Vec::new();
                for (target, input) in targets {
                    if is_local(target) {
                        routes.push_str(&format!("out {output} {target} {input}\n"));
                    } else if !machines.contains(&machine_of[target]) {
                        machines.push(machine_of[target].clone());
                    }
                }
                if !machines.is_empty() {
                    routes.push_str(&format!("out {output} {DAEMON}\n"));
                    let path = channel::forward_path(dir, source, output);
                    let channel = Channel::create(&path, channel::Keep::All)?;
                    let downstream = downstream_machines.entry(source.clone()).or_default();
                    downstream.extend(machines.iter().filter(|m| !downstream.contains(m)).cloned().collect::<Vec<_>>());
                    forwards.push(Forward { source: source.clone(), output: output.clone(), channel, machines });
                }
            } else {
                for (target, input) in targets.iter().filter(|(t, _)| is_local(t)) {
                    let target_files = Target {
                        channel: Channel::open(&channel::channel_path(dir, target, input))?,
                        bell: Arc::new(Bell::open(&channel::bell_path(dir, target))?),
                    };
                    let key = (source.clone(), output.clone());
                    inbound.entry(key).or_default().push((target.clone(), input.clone(), target_files));
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
                sockets: HashMap::new(),
                bells,
                upstream: (graph.upstream.into_iter()).filter(|(id, _)| is_local(id)).collect(),
                nodes: (local_nodes.iter())
                    .map(|n| {
                        let info =
                            NodeInfo { pid: None, state: NodeState::Starting, stop_sent: None, terminated: false };
                        (n.id.clone(), info)
                    })
                    .collect(),
                logs: VecDeque::new(),
                next_log: 0,
                forward_logs: config.machine.is_some().then(|| events.clone()),
                stop_requested: false,
                stopping: false,
                aborted: false,
                closed: false,
            }),
            routes: graph.routes,
            cyclic: graph.cyclic,
            node_index,
            node_routes,
            tracing,
            machine: config.machine,
            machine_of,
            addresses: config.dataflow.machines,
            downstream_machines,
            forwards,
            inbound,
            daemon_bell,
            closing: Mutex::new(Vec::new()),
            peers: Mutex::new(HashMap::new()),
            carried: AtomicU64::new(0),
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
        {
            let session = session.clone();
            std::thread::spawn(move || session.forward_loop());
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
        let ids: Vec<String> = s.sockets.keys().cloned().collect();
        for id in ids {
            let routes = self.node_routes.get(&id).cloned().unwrap_or_default();
            let _ = DaemonMsg::Ready { routes }.write_to(s.sockets.get_mut(&id).unwrap());
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
        // Message counts come from the senders' stats files.
        let sent: HashMap<(String, String), (u64, u64)> =
            (self.tracing.outputs().into_iter()).map(|(node, output, n, bytes)| ((node, output), (n, bytes))).collect();
        let s = self.state.lock().unwrap();
        let nodes = (s.nodes.iter())
            .map(|(id, info)| {
                let regions = self.regions_of(id);
                NodeStatus {
                    id: id.clone(),
                    machine: self.machine.clone(),
                    pid: info.pid,
                    state: info.state.clone(),
                    shm_regions: regions.len() as u32,
                    shm_held: regions.iter().filter(|r| r.refcount().load(Ordering::Relaxed) > 0).count() as u32,
                    shm_bytes: regions.iter().map(|r| r.capacity() as u64).sum(),
                }
            })
            .collect();
        let endpoint = |node: &str, port: &str| match self.machine_of.get(node) {
            Some(machine) if Some(machine) != self.machine.as_ref() => format!("{node}/{port}@{machine}"),
            _ => format!("{node}/{port}"),
        };
        let mut links: Vec<LinkStatus> = (self.routes.iter())
            .filter(|((node, _), _)| s.nodes.contains_key(node))
            .map(|((node, output), targets)| {
                let (messages, bytes) = sent.get(&(node.clone(), output.clone())).copied().unwrap_or_default();
                LinkStatus {
                    source: endpoint(node, output),
                    targets: targets.iter().map(|(n, i)| endpoint(n, i)).collect(),
                    messages,
                    bytes,
                }
            })
            .collect();
        links.sort_by(|a, b| a.source.cmp(&b.source));
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

    /// The regions a node has created so far, freshly mapped.
    fn regions_of(&self, node: &str) -> Vec<Region> {
        (0..shm::MAX_SLOTS as u32)
            .map_while(|slot| Region::open(&shm::region_path(&self.shm_dir, node, slot)).ok())
            .collect()
    }

    /// Starts each node in its own process group, with its output captured
    /// and its real-time settings applied.
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
            if node.rt.is_some() {
                command.env(ENV_REALTIME, "1");
            }
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
            if let Some(rt) = &node.rt {
                for problem in apply_realtime(child.id(), rt) {
                    self.log("daemon", format!("`{}`: {problem}", node.id));
                }
            }

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

    /// A node's connection: registration, then nothing until it closes when
    /// the node exits.
    fn handle_node(&self, mut stream: UnixStream) {
        let node_id = match NodeMsg::read_from(&mut stream) {
            Ok(Some(NodeMsg::Register { node_id })) => node_id,
            // The wake-up connection from `cleanup`, or a stray client.
            _ => return,
        };
        {
            let mut s = self.state.lock().unwrap();
            if !s.expected.contains(&node_id) || s.sockets.contains_key(&node_id) {
                s.log("daemon", format!("rejecting unknown or duplicate node `{node_id}`"));
                return;
            }
            let Ok(writer) = stream.try_clone() else { return };
            s.sockets.insert(node_id.clone(), writer);
            if s.sockets.len() == s.expected.len() {
                let count = s.expected.len();
                s.log("daemon", format!("all {count} local nodes registered"));
                drop(s);
                self.emit(Event::AllRegistered);
            }
        }
        match NodeMsg::read_from(&mut stream) {
            // The node exited; `supervise` reports how.
            Ok(None) | Err(_) => {}
            Ok(Some(other)) => self.log("daemon", format!("unexpected message from `{node_id}`: {other:?}")),
        }
        self.node_disconnected(&node_id);
    }

    /// Forwards what local nodes send to other machines, until the session
    /// closes. Tells other machines when a node is gone, once everything it
    /// sent is on its way.
    fn forward_loop(&self) {
        // Senders' regions, by `(node, slot)`.
        let mut regions: HashMap<(String, u32), Arc<Region>> = HashMap::new();
        while !self.is_closed() {
            let seen = self.daemon_bell.seq();
            // Taken first: these nodes exited, so all they sent is in the
            // channels drained next.
            let closing = std::mem::take(&mut *self.closing.lock().unwrap());
            let mut forwarded = false;
            for forward in &self.forwards {
                while let Some((slot, len)) = forward.channel.pop() {
                    forwarded = true;
                    self.forward(forward, slot, len as usize, &mut regions);
                }
            }
            for node in &closing {
                for machine in self.downstream_machines.get(node).into_iter().flatten() {
                    let Some(peer) = self.peers.lock().unwrap().get(machine).cloned() else { continue };
                    let _ = PeerMsg::Closed { node: node.clone() }.write_to(&mut *peer.lock().unwrap());
                }
            }
            if !forwarded && closing.is_empty() {
                self.daemon_bell.wait(seen, Some(Duration::from_millis(100)));
            }
        }
    }

    fn forward(&self, forward: &Forward, slot: u32, len: usize, regions: &mut HashMap<(String, u32), Arc<Region>>) {
        let key = (forward.source.clone(), slot);
        let region = match regions.get(&key) {
            Some(region) if region.capacity() >= len => region.clone(),
            _ => match Region::open(&shm::region_path(&self.shm_dir, &forward.source, slot)) {
                Ok(region) => {
                    let region = Arc::new(region);
                    regions.insert(key, region.clone());
                    region
                }
                // The session is over and removed its files.
                Err(_) if self.is_closed() => return,
                Err(e) => {
                    self.log(
                        "daemon",
                        format!("`{}` sent from region {slot}, which can't be opened: {e}", forward.source),
                    );
                    return;
                }
            },
        };
        let context = region.context();
        if context.sampled {
            self.tracing.routed(&context, &forward.source, &forward.output, trace::now_ns());
        }
        // SAFETY: we hold the reference the sender took for us.
        let payload = unsafe { region.payload_slice(len) };
        for machine in &forward.machines {
            let Some(peer) = self.peers.lock().unwrap().get(machine).cloned() else { continue };
            let written =
                PeerMsg::write_data(&mut *peer.lock().unwrap(), &forward.source, &forward.output, &context, payload);
            if let Err(e) = written {
                self.peers.lock().unwrap().remove(machine);
                self.log("daemon", format!("lost the data connection to machine `{machine}`: {e}"));
            } else if context.sampled {
                self.tracing.net_sent(&context, machine, trace::now_ns());
            }
        }
        region.refcount().fetch_sub(1, Ordering::Release);
        self.carried.fetch_add(1, Ordering::Relaxed);
    }

    /// Receives what a daemon on another machine forwards to our nodes, and
    /// hands it to them as if it had been sent here.
    pub fn serve_peer(&self, stream: TcpStream) {
        let mut reader = BufReader::with_capacity(1 << 20, stream);
        // Regions standing in for each remote source, named after it.
        let mut pools: HashMap<String, Pool> = HashMap::new();
        loop {
            match PeerMsg::read_from(&mut reader) {
                Ok(Some(PeerMsg::Data { source, output, mut context, payload })) => {
                    let received = trace::now_ns();
                    context.published_ns = self.tracing.to_local(context.published_ns, &source);
                    let targets = match self.inbound.get(&(source.clone(), output.clone())) {
                        Some(targets) => targets,
                        None => continue,
                    };
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
                    if context.sampled {
                        self.tracing.net_received(&context, &source, &output, received);
                    }
                    let files: Vec<&Target> = targets.iter().map(|(_, _, t)| t).collect();
                    send_to(&files, slot, payload.len() as u64, pool);
                    self.carried.fetch_add(1, Ordering::Relaxed);
                    if context.sampled {
                        let now = trace::now_ns();
                        for (node, input, _) in targets {
                            self.tracing.delivered(&context, node, input, now);
                        }
                    }
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

    /// `node_id` (local, or on another machine) is gone: stops local nodes
    /// that have no upstream left, and has the forwarder tell machines
    /// downstream.
    fn node_disconnected(&self, node_id: &str) {
        {
            let mut s = self.state.lock().unwrap();
            s.sockets.remove(node_id);
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
        if self.downstream_machines.contains_key(node_id) {
            self.closing.lock().unwrap().push(node_id.to_owned());
            self.daemon_bell.ring();
        }
    }

    /// Waits for all local nodes to exit. If one fails, kills the others.
    ///
    /// Stopping drains the dataflow: sources and nodes on cycles get `Stop`
    /// first, and each other node gets it once all its upstream nodes have
    /// exited, so no in-flight message is dropped. If that stalls for
    /// `DRAIN_TIMEOUT` (no message sent, no node gone), every node gets it. A
    /// node that ignores `Stop` gets SIGTERM, then SIGKILL.
    fn supervise(&self, mut children: Vec<(String, Child)>) -> bool {
        let mut ok = true;
        let mut stop_started: Option<Instant> = None;
        let mut drained = false;
        let (mut last_progress, mut last_sent) = (Instant::now(), 0);
        while !children.is_empty() {
            let mut i = 0;
            while i < children.len() {
                match children[i].1.try_wait() {
                    Ok(Some(status)) => {
                        let (id, _) = children.remove(i);
                        last_progress = Instant::now();
                        let mut s = self.state.lock().unwrap();
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
            if stop_started.is_some() {
                let sent = self.tracing.messages_sent() + self.carried.load(Ordering::Relaxed);
                if sent != last_sent {
                    (last_sent, last_progress) = (sent, Instant::now());
                }
            }
            let mut s = self.state.lock().unwrap();
            if s.stopping && stop_started.is_none() {
                stop_started = Some(Instant::now());
                last_progress = Instant::now();
                s.log("daemon", "stopping the dataflow, starting with its sources".into());
                let sources: Vec<String> = (s.upstream.iter())
                    .filter(|(id, up)| up.is_empty() || self.cyclic.contains(*id))
                    .map(|(id, _)| id.clone())
                    .collect();
                for id in sources {
                    send_stop(&mut s, &id);
                }
            }
            if !drained && stop_started.is_some() && last_progress.elapsed() > DRAIN_TIMEOUT {
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
        self.daemon_bell.ring();
        if let Some(files) = self.files.lock().unwrap().take() {
            // Wakes the accept loop so it sees `closed` and lets go of us.
            let _ = UnixStream::connect(&files.nodes_socket);
        }
    }
}

/// Like `channel::send`, for the regions of a pool the session fills.
fn send_to(targets: &[&Target], slot: u32, len: u64, pool: &Pool) {
    pool.region(slot).refcount().store(targets.len() as u32, Ordering::Release);
    for target in targets {
        if let Some((dropped, _)) = target.channel.push(slot, len) {
            pool.region(dropped).refcount().fetch_sub(1, Ordering::Release);
        }
        target.bell.ring();
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
    if let Some(bell) = s.bells.get(node_id) {
        bell.request_stop();
    }
}

/// Pins a node to its CPUs and gives it SCHED_FIFO. Returns what couldn't be
/// done: without privileges, real-time priority is refused.
fn apply_realtime(pid: u32, rt: &Realtime) -> Vec<String> {
    let mut problems = Vec::new();
    // SAFETY: plain syscalls on a child we just spawned.
    unsafe {
        if !rt.cpus.is_empty() {
            let mut set: libc::cpu_set_t = std::mem::zeroed();
            for &cpu in &rt.cpus {
                libc::CPU_SET(cpu, &mut set);
            }
            if libc::sched_setaffinity(pid as i32, std::mem::size_of::<libc::cpu_set_t>(), &set) != 0 {
                problems.push(format!("can't pin to CPUs {:?}: {}", rt.cpus, io::Error::last_os_error()));
            }
        }
        if let Some(priority) = rt.priority {
            // Zeroed first: musl's has more fields than glibc's.
            let mut param: libc::sched_param = std::mem::zeroed();
            param.sched_priority = priority as i32;
            // The raw syscall: musl's wrapper always fails with ENOSYS, since
            // Linux sets it per thread (here, the node's main thread).
            let pid = pid as libc::pid_t;
            if libc::syscall(libc::SYS_sched_setscheduler, pid, libc::SCHED_FIFO, &param as *const libc::sched_param)
                != 0
            {
                problems.push(format!(
                    "can't get real-time priority {priority}, running with normal scheduling: {} \
                     (allow it with an rtprio limit in /etc/security/limits.d, or CAP_SYS_NICE)",
                    io::Error::last_os_error()
                ));
            }
        }
    }
    problems
}
