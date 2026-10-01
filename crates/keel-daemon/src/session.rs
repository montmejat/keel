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
//! SIGKILL), and makes sure they die with it. It restarts nodes whose policy
//! says so, kills those its watchdog finds stuck, and replaces nodes one at
//! a time when a new deployment comes in. What happens is reported as
//! [`Event`]s, to `keel run` or to the coordinator.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use keel::channel::{self, Bell, Channel, Target, DAEMON};
use keel::protocol::{
    DaemonMsg, NodeMsg, PeerMsg, ENV_DAEMON_SOCKET, ENV_DATAFLOW, ENV_DEPLOYMENT, ENV_NODE_ID, ENV_REALTIME,
    ENV_SHM_DIR,
};
use keel::shm::{self, HeldTable, Pool, Region};
use keel::trace::{self, ENV_NODE_INDEX};

use crate::control::{self, Clock, LinkStatus, LogLine, NodeState, NodeStatus, Reply, Request, Status};
use crate::dataflow::{Dataflow, NodeConfig, Realtime, Restart, Routes};
use crate::runtime::SessionFiles;
use crate::store::Store;
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
/// Restarts wait this long, doubling each time, up to `MAX_BACKOFF`.
const FIRST_BACKOFF: Duration = Duration::from_millis(100);
const MAX_BACKOFF: Duration = Duration::from_secs(5);
/// A node that ran this long since its last start gets its restarts back.
const STABLE_AFTER: Duration = Duration::from_secs(60);
/// Local `(node, input, channel)`s fed by each remote `(node, output)`.
type Inbound = HashMap<(String, String), Vec<(String, String, Target)>>;

pub(crate) struct SessionConfig {
    /// The dataflow file, for display.
    pub name: PathBuf,
    /// `None` when the whole dataflow runs here.
    pub machine: Option<String>,
    pub dataflow: Dataflow,
    /// `path:` nodes are relative to this.
    pub base_dir: PathBuf,
    /// What `build:` nodes run: their binary in the store.
    pub executables: BTreeMap<String, PathBuf>,
    /// The deployment this runs, if any: nodes are told.
    pub deployment: Option<String>,
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
    dataflow_name: String,
    /// The deployment running; an update replaces it.
    deployment: Mutex<Option<String>>,
    /// What each local node runs, for display.
    programs: HashMap<String, String>,
    /// Local nodes, to spawn them again.
    configs: HashMap<String, NodeConfig>,
    base_dir: PathBuf,
    /// What `build:` nodes run; an update replaces entries.
    executables: Mutex<BTreeMap<String, PathBuf>>,
    /// Source node of each local node's inputs, in their stats-file order:
    /// to release what a dead node held.
    input_sources: HashMap<String, Vec<String>>,
    output_threads: Mutex<Vec<JoinHandle<()>>>,
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
    /// Registered nodes' connections, to send them their routes, numbered so
    /// that a restarted node's old connection closing doesn't drop its new one.
    sockets: HashMap<String, (u64, UnixStream)>,
    next_connection: u64,
    /// `start` has run: nodes registering from now on are restarts.
    started: bool,
    /// Nodes to replace with their new binary, one at a time.
    updates: VecDeque<String>,
    updating: Option<String>,
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
    restarts: u32,
    started: Instant,
    /// Being replaced by an update: start it again whatever its policy.
    replacing: bool,
}

impl State {
    fn log(&mut self, node: &str, text: String) {
        // Never panic here, under the session's lock: a closed stderr (the
        // terminal gone, a pipe's reader exited) must not take the session down.
        let _ = writeln!(io::stderr(), "[{node}] {text}");
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
                let (source_node, output) = source.source().split_once('/').unwrap();
                routes.push_str(&format!("in {input} {source_node} {output}\n"));
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
                name: config.name.clone(),
                start: Instant::now(),
                expected: local_nodes.iter().map(|n| n.id.clone()).collect(),
                sockets: HashMap::new(),
                next_connection: 0,
                started: false,
                updates: VecDeque::new(),
                updating: None,
                bells,
                upstream: (graph.upstream.into_iter()).filter(|(id, _)| is_local(id)).collect(),
                nodes: (local_nodes.iter())
                    .map(|n| {
                        let info = NodeInfo {
                            pid: None,
                            state: NodeState::Starting,
                            stop_sent: None,
                            terminated: false,
                            restarts: 0,
                            started: Instant::now(),
                            replacing: false,
                        };
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
            dataflow_name: config.name.file_stem().map_or(String::new(), |s| s.to_string_lossy().into_owned()),
            deployment: Mutex::new(config.deployment.clone()),
            configs: local_nodes.iter().map(|n| (n.id.clone(), n.clone())).collect(),
            base_dir: config.base_dir.clone(),
            executables: Mutex::new(config.executables.clone()),
            input_sources: (local_nodes.iter())
                .map(|n| {
                    let sources = n.inputs.values().map(|i| i.source().split('/').next().unwrap().to_owned());
                    (n.id.clone(), sources.collect())
                })
                .collect(),
            output_threads: Mutex::new(Vec::new()),
            programs: (local_nodes.iter())
                .map(|n| {
                    let file = n.path.as_ref().and_then(|p| p.file_name()).map(|f| f.to_string_lossy().into_owned());
                    (n.id.clone(), n.build.clone().or(file).unwrap_or_default())
                })
                .collect(),
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
                let mut children = Vec::new();
                for node in &local_nodes {
                    match session.spawn_one(node) {
                        Ok(child) => children.push((node.id.clone(), child)),
                        Err(e) => {
                            session.kill_all(&mut children);
                            session.cleanup();
                            let _ = spawned_tx.send(Err(e));
                            return;
                        }
                    }
                }
                let _ = spawned_tx.send(Ok(()));
                if local_nodes.is_empty() {
                    session.emit(Event::AllRegistered);
                }
                let ok = session.supervise(children);
                let threads = std::mem::take(&mut *session.output_threads.lock().unwrap());
                for thread in threads {
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
            let stream =
                wire::open(address, wire::PEER).map_err(|e| io::Error::other(format!("machine `{machine}`: {e}")))?;
            self.peers.lock().unwrap().insert(machine.clone(), Arc::new(Mutex::new(stream)));
        }
        let mut s = self.state.lock().unwrap();
        s.started = true;
        let ids: Vec<String> = s.sockets.keys().cloned().collect();
        for id in ids {
            let routes = self.node_routes.get(&id).cloned().unwrap_or_default();
            let _ = DaemonMsg::Ready { routes }.write_to(&mut s.sockets.get_mut(&id).unwrap().1);
            s.set_state(&id, NodeState::Running);
        }
        Ok(())
    }

    /// Replaces local nodes' binaries with a new deployment's, restarting the
    /// nodes whose binary changed one at a time. Returns those nodes.
    pub fn update(&self, deployment: Option<String>, binaries: &BTreeMap<String, String>) -> io::Result<Vec<String>> {
        let store = Store::open()?;
        let mut changed = Vec::new();
        {
            let mut executables = self.executables.lock().unwrap();
            for (node, hash) in binaries.iter().filter(|(node, _)| self.configs.contains_key(*node)) {
                let path = store.blob(hash)?;
                if !path.exists() {
                    return Err(io::Error::other(format!("`{node}`'s new binary {hash} isn't in the store")));
                }
                if executables.get(node) != Some(&path) {
                    executables.insert(node.clone(), path);
                    changed.push(node.clone());
                }
            }
        }
        *self.deployment.lock().unwrap() = deployment;
        let mut s = self.state.lock().unwrap();
        if !changed.is_empty() {
            s.log("daemon", format!("updating {}, one at a time", changed.join(", ")));
        }
        s.updates.extend(changed.iter().cloned());
        Ok(changed)
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
            Request::Trace { summary } => Reply::Trace(self.tracing.report(!summary)),
            Request::Update { deployment, binaries } => match self.update(deployment, &binaries) {
                Ok(nodes) => Reply::Updating(nodes),
                Err(e) => Reply::Error(e.to_string()),
            },
        }
    }

    fn status(&self) -> Status {
        // Message counts come from the senders' stats files.
        let sent: HashMap<(String, String), (u64, u64)> =
            (self.tracing.outputs().into_iter()).map(|(node, output, n, bytes)| ((node, output), (n, bytes))).collect();
        let s = self.state.lock().unwrap();
        let mut nodes: Vec<NodeStatus> = (s.nodes.iter())
            .map(|(id, info)| {
                let regions = self.regions_of(id);
                NodeStatus {
                    id: id.clone(),
                    machine: self.machine.clone(),
                    program: self.programs.get(id).cloned().unwrap_or_default(),
                    pid: info.pid,
                    state: info.state.clone(),
                    restarts: info.restarts,
                    shm_regions: regions.len() as u32,
                    shm_held: regions.iter().filter(|r| r.refcount().load(Ordering::Relaxed) > 0).count() as u32,
                    shm_bytes: regions.iter().map(|r| r.capacity() as u64).sum(),
                }
            })
            .collect();
        nodes.sort_by_key(|n| self.node_index.get(&n.id).copied());
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
            deployment: self.deployment.lock().unwrap().clone(),
        }
    }

    /// The regions a node has created so far, freshly mapped.
    fn regions_of(&self, node: &str) -> Vec<Region> {
        (0..shm::MAX_SLOTS as u32)
            .map_while(|slot| Region::open(&shm::region_path(&self.shm_dir, node, slot)).ok())
            .collect()
    }

    /// Starts a node in its own process group, with its output captured and
    /// its real-time settings applied.
    fn spawn_one(self: &Arc<Self>, node: &NodeConfig) -> io::Result<Child> {
        let (nodes_socket, shm_dir) = {
            let files = self.files.lock().unwrap();
            let files = files.as_ref().ok_or_else(|| io::Error::other("the session is over"))?;
            (files.nodes_socket.clone(), files.shm_dir.clone())
        };
        let exe = match (&node.path, self.executables.lock().unwrap().get(&node.id)) {
            (_, Some(exe)) => exe.clone(),
            (Some(path), None) => self.base_dir.join(path),
            (None, None) => {
                return Err(io::Error::other(format!("`{}` is a `build:` node but wasn't deployed", node.id)));
            }
        };
        let mut command = Command::new(&exe);
        command
            .env(ENV_NODE_ID, &node.id)
            .env(ENV_NODE_INDEX, self.node_index[&node.id].to_string())
            .env(ENV_DAEMON_SOCKET, &nodes_socket)
            .env(ENV_SHM_DIR, &shm_dir)
            .env(ENV_DATAFLOW, &self.dataflow_name)
            .args(&node.args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Keeps Ctrl-C in the terminal from reaching nodes directly:
            // the session decides how they stop.
            .process_group(0);
        if node.rt.is_some() {
            command.env(ENV_REALTIME, "1");
        }
        if let Some(deployment) = &*self.deployment.lock().unwrap() {
            command.env(ENV_DEPLOYMENT, deployment);
        }
        // SAFETY: prctl is async-signal-safe.
        unsafe {
            command.pre_exec(|| match libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) {
                -1 => Err(io::Error::last_os_error()),
                _ => Ok(()),
            });
        }
        let mut child = command
            .spawn()
            .map_err(|e| io::Error::other(format!("failed to spawn `{}` ({}): {e}", node.id, exe.display())))?;
        {
            let mut s = self.state.lock().unwrap();
            let info = s.nodes.get_mut(&node.id).unwrap();
            (info.pid, info.state, info.started) = (Some(child.id()), NodeState::Starting, Instant::now());
        }
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
        let (stdout, stderr) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
        let mut threads = self.output_threads.lock().unwrap();
        threads.push(capture(Box::new(stdout)));
        threads.push(capture(Box::new(stderr)));
        Ok(child)
    }

    /// A node's connection: registration, then nothing until it closes when
    /// the node exits. A node registering once the dataflow runs (a restart)
    /// gets its routes right away.
    fn handle_node(&self, mut stream: UnixStream) {
        let node_id = match NodeMsg::read_from(&mut stream) {
            Ok(Some(NodeMsg::Register { node_id })) => node_id,
            // The wake-up connection from `cleanup`, or a stray client.
            _ => return,
        };
        let connection = {
            let mut s = self.state.lock().unwrap();
            if !s.expected.contains(&node_id) || s.sockets.contains_key(&node_id) {
                s.log("daemon", format!("rejecting unknown or duplicate node `{node_id}`"));
                return;
            }
            let Ok(mut writer) = stream.try_clone() else { return };
            let connection = s.next_connection;
            s.next_connection += 1;
            if s.started {
                let routes = self.node_routes.get(&node_id).cloned().unwrap_or_default();
                let _ = DaemonMsg::Ready { routes }.write_to(&mut writer);
                s.set_state(&node_id, NodeState::Running);
                if s.updating.as_ref() == Some(&node_id) {
                    s.updating = None;
                    s.log("daemon", format!("`{node_id}` runs its new binary"));
                }
                s.sockets.insert(node_id.clone(), (connection, writer));
            } else {
                s.sockets.insert(node_id.clone(), (connection, writer));
                if s.sockets.len() == s.expected.len() {
                    let count = s.expected.len();
                    s.log("daemon", format!("all {count} local nodes registered"));
                    drop(s);
                    self.emit(Event::AllRegistered);
                }
            }
            connection
        };
        match NodeMsg::read_from(&mut stream) {
            // The node exited; `supervise` reports how.
            Ok(None) | Err(_) => {}
            Ok(Some(other)) => self.log("daemon", format!("unexpected message from `{node_id}`: {other:?}")),
        }
        let mut s = self.state.lock().unwrap();
        if s.sockets.get(&node_id).is_some_and(|(c, _)| *c == connection) {
            s.sockets.remove(&node_id);
        }
    }

    /// Gives back the references a dead node still held, so that its
    /// senders get their regions back.
    fn release_held(&self, node: &str) {
        let Ok(table) = HeldTable::open(&self.shm_dir, node) else { return };
        let held = table.held();
        for &(input, slot) in &held {
            let Some(source) = self.input_sources.get(node).and_then(|s| s.get(input)) else { continue };
            if let Ok(region) = Region::open(&shm::region_path(&self.shm_dir, source, slot)) {
                region.refcount().fetch_sub(1, Ordering::Release);
            }
        }
        table.clear();
        if !held.is_empty() {
            self.log("daemon", format!("released {} messages `{node}` held when it died", held.len()));
        }
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
                Ok(Some(PeerMsg::Closed { node })) => self.node_gone(&node),
                Ok(None) => break,
                Err(e) => {
                    self.log("daemon", format!("data connection from another machine failed: {e}"));
                    break;
                }
            }
        }
    }

    /// `node_id` (local, or on another machine) is gone for good: stops local
    /// nodes that have no upstream left, and has the forwarder tell machines
    /// downstream.
    fn node_gone(&self, node_id: &str) {
        {
            let mut s = self.state.lock().unwrap();
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

    /// Waits for all local nodes to exit for good, restarting those whose
    /// policy says so. If one fails for good, kills the others.
    ///
    /// Stopping drains the dataflow: sources and nodes on cycles get `Stop`
    /// first, and each other node gets it once all its upstream nodes have
    /// exited, so no in-flight message is dropped. If that stalls for
    /// `DRAIN_TIMEOUT` (no message sent, no node gone), every node gets it. A
    /// node that ignores `Stop` gets SIGTERM, then SIGKILL.
    fn supervise(self: &Arc<Self>, mut children: Vec<(String, Child)>) -> bool {
        let mut ok = true;
        let mut stop_started: Option<Instant> = None;
        let mut drained = false;
        let (mut last_progress, mut last_sent) = (Instant::now(), 0);
        // Nodes to start again, and when.
        let mut restarts: Vec<(Instant, String)> = Vec::new();
        while !children.is_empty() || !restarts.is_empty() {
            let mut i = 0;
            while i < children.len() {
                match children[i].1.try_wait() {
                    Ok(Some(status)) => {
                        let (id, _) = children.remove(i);
                        last_progress = Instant::now();
                        // Whatever it held goes back to its senders, before a
                        // new instance starts over with a fresh table.
                        self.release_held(&id);
                        match self.after_exit(&id, status) {
                            Some(delay) => restarts.push((Instant::now() + delay, id)),
                            None => {
                                let success = matches!(
                                    self.state.lock().unwrap().nodes[&id].state,
                                    NodeState::Exited { success: true, .. }
                                );
                                self.emit(Event::NodeExited { node: id.clone(), success });
                                self.node_gone(&id);
                                if !success {
                                    ok = false;
                                    if !self.state.lock().unwrap().stopping {
                                        self.log("daemon", format!("stopping the dataflow because `{id}` failed"));
                                        self.kill_all(&mut children);
                                        restarts.clear();
                                    }
                                }
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

            // Restarts that are due.
            let now = Instant::now();
            let (due, later): (Vec<_>, Vec<_>) = restarts.drain(..).partition(|(at, _)| *at <= now);
            restarts = later;
            for (_, id) in due {
                if self.state.lock().unwrap().stopping {
                    // Too late: count it as gone.
                    self.state
                        .lock()
                        .unwrap()
                        .set_state(&id, NodeState::Exited { success: true, detail: "not restarted".into() });
                    self.node_gone(&id);
                    continue;
                }
                match self.spawn_one(&self.configs[&id]) {
                    Ok(child) => children.push((id, child)),
                    Err(e) => {
                        self.log("daemon", format!("can't restart `{id}`: {e}"));
                        self.state
                            .lock()
                            .unwrap()
                            .set_state(&id, NodeState::Exited { success: false, detail: e.to_string() });
                        self.emit(Event::NodeExited { node: id.clone(), success: false });
                        self.node_gone(&id);
                        ok = false;
                    }
                }
            }

            self.watchdog(&mut children);
            self.next_update(&children);

            if self.state.lock().unwrap().aborted && !children.is_empty() {
                self.log("daemon", "aborting, killing all nodes".into());
                self.kill_all(&mut children);
                restarts.clear();
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

    /// Records a node's exit. Returns when to start it again, if it should
    /// be: it's being updated, or its policy says so and it has restarts left
    /// (and nothing told it to stop).
    fn after_exit(&self, id: &str, status: std::process::ExitStatus) -> Option<Duration> {
        let config = &self.configs[id];
        let mut s = self.state.lock().unwrap();
        let told_to_stop = s.bells.get(id).is_some_and(|b| b.stop_requested());
        let (stopping, aborted) = (s.stopping, s.aborted);
        let info = s.nodes.get_mut(id).unwrap();
        // A node we terminated to stop it did what it was asked.
        let terminated = info.terminated && status.signal() == Some(libc::SIGTERM);
        let success = status.success() || terminated;
        let replacing = std::mem::take(&mut info.replacing);
        if info.started.elapsed() > STABLE_AFTER {
            info.restarts = 0;
        }
        let wanted = replacing
            || match config.restart {
                Restart::Never => false,
                Restart::OnFailure => !success,
                Restart::Always => true,
            };
        let restart =
            wanted && !stopping && !aborted && !told_to_stop && (replacing || info.restarts < config.max_restarts);
        if !restart {
            info.state = NodeState::Exited { success, detail: status.to_string() };
            let gave_up = wanted && !stopping && !aborted && !told_to_stop;
            s.log(
                "daemon",
                match gave_up {
                    true => format!("`{id}` exited: {status}, after {} restarts: giving up", config.max_restarts),
                    false => format!("`{id}` exited: {status}"),
                },
            );
            return None;
        }
        (info.terminated, info.stop_sent, info.pid) = (false, None, None);
        info.state = NodeState::Starting;
        if replacing {
            s.log("daemon", format!("`{id}` exited for the update: {status}, starting its new binary"));
            return Some(Duration::ZERO);
        }
        info.restarts += 1;
        let delay = (FIRST_BACKOFF * 2u32.saturating_pow(info.restarts - 1)).min(MAX_BACKOFF);
        let n = info.restarts;
        s.log("daemon", format!("`{id}` exited: {status}; restarting in {delay:?} ({n} of {})", config.max_restarts));
        Some(delay)
    }

    /// Kills running nodes that have neither taken nor sent a message for
    /// longer than their `watchdog_ms`, while not waiting for anything. They
    /// count as failed: restarted if their policy says so.
    fn watchdog(&self, children: &mut [(String, Child)]) {
        let now = trace::now_ns();
        for (id, child) in children.iter_mut() {
            let Some(limit_ms) = self.configs[id].watchdog_ms else { continue };
            let Some((activity, waiting)) = self.tracing.activity(id) else { continue };
            let running = matches!(self.state.lock().unwrap().nodes[id].state, NodeState::Running);
            let idle_ms = now.saturating_sub(activity) / 1_000_000;
            if running && activity > 0 && !waiting && idle_ms > limit_ms {
                self.log(
                    "daemon",
                    format!("`{id}` made no progress for {idle_ms} ms (watchdog: {limit_ms} ms), killing it"),
                );
                let _ = child.kill();
            }
        }
    }

    /// Starts replacing the next node of an update, once the previous one
    /// runs its new binary: asks it to exit (SIGTERM); it starts again with
    /// the new binary, and the messages queued for it wait in its channels.
    fn next_update(&self, children: &[(String, Child)]) {
        let mut s = self.state.lock().unwrap();
        if s.updating.is_some() || s.stopping {
            return;
        }
        while let Some(id) = s.updates.pop_front() {
            let Some((_, child)) = children.iter().find(|(c, _)| *c == id) else { continue };
            s.nodes.get_mut(&id).unwrap().replacing = true;
            s.updating = Some(id.clone());
            s.log("daemon", format!("replacing `{id}`"));
            // SAFETY: plain syscall on a child we haven't reaped yet.
            unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
            return;
        }
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
