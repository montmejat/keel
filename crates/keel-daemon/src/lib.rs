//! keel-daemon: runs one dataflow on this machine.
//!
//! Spawns every node, waits for all of them to register, then routes each
//! output to the inputs subscribed to it. A node gets `Stop` once all of its
//! upstream nodes have exited. The daemon exits when all nodes have.
//!
//! Payloads never pass through the daemon: nodes exchange shared-memory
//! regions, and the daemon forwards descriptors and keeps the regions'
//! reference counts right (see `keel::shm`).
//!
//! The daemon acts as a small init for its nodes: it captures their output,
//! stops them in dataflow order on SIGINT/SIGTERM or a control request
//! (`Stop`, then SIGTERM, then SIGKILL), and makes sure they die with it. Tools inspect it through
//! the control API (see `control`).

pub mod control;
pub mod dataflow;
pub mod runtime;
mod signals;

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::io::{self, BufRead, BufReader, Read};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use control::{LinkStatus, LogLine, NodeState, NodeStatus, Reply, Request, Status};
use dataflow::{Dataflow, Graph, NodeConfig, Routes};
use keel::protocol::{DaemonMsg, NodeMsg, ENV_DAEMON_SOCKET, ENV_NODE_ID, ENV_SHM_DIR};
use keel::shm::{self, Region};
use runtime::RuntimeFiles;

/// After `Stop`, how long a node gets to exit before SIGTERM, then SIGKILL.
const STOP_GRACE: Duration = Duration::from_secs(2);
const TERM_GRACE: Duration = Duration::from_secs(3);
/// After a stop request, how long `Stop` may take to cascade from the sources.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
const LOG_CAPACITY: usize = 10_000;

struct State {
    dataflow: PathBuf,
    start: Instant,
    routes: Arc<Routes>,
    expected: HashSet<String>,
    writers: HashMap<String, Arc<Mutex<UnixStream>>>,
    /// Nodes still running upstream of each node.
    upstream: HashMap<String, HashSet<String>>,
    nodes: BTreeMap<String, NodeInfo>,
    /// Messages and bytes sent, by `(node, output)`.
    counters: BTreeMap<(String, String), (u64, u64)>,
    logs: VecDeque<LogLine>,
    next_log: u64,
    stop_requested: bool,
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
        self.logs.push_back(LogLine { seq: self.next_log, t_ms, node: node.to_owned(), text });
        self.next_log += 1;
    }

    fn set_state(&mut self, node: &str, state: NodeState) {
        if let Some(info) = self.nodes.get_mut(node) {
            info.state = state;
        }
    }

    fn status(&self) -> Status {
        let nodes = self
            .nodes
            .iter()
            .map(|(id, info)| NodeStatus {
                id: id.clone(),
                pid: info.pid,
                state: info.state.clone(),
                shm_regions: info.regions.len() as u32,
                shm_held: info.regions.iter().filter(|r| r.refcount().load(Ordering::Relaxed) > 0).count() as u32,
                shm_bytes: info.regions.iter().map(|r| r.capacity() as u64).sum(),
            })
            .collect();
        let links = self
            .counters
            .iter()
            .map(|((node, output), &(messages, bytes))| LinkStatus {
                source: format!("{node}/{output}"),
                targets: (self.routes.get(&(node.clone(), output.clone())).into_iter().flatten())
                    .map(|(n, i)| format!("{n}/{i}"))
                    .collect(),
                messages,
                bytes,
            })
            .collect();
        Status {
            pid: std::process::id(),
            dataflow: self.dataflow.clone(),
            uptime_ms: self.start.elapsed().as_millis() as u64,
            stopping: self.stop_requested,
            nodes,
            links,
        }
    }

    fn handle(&mut self, request: Request) -> Reply {
        match request {
            Request::Status => Reply::Status(self.status()),
            Request::Logs { since } => {
                let lines = self.logs.iter().filter(|l| l.seq >= since).cloned().collect();
                Reply::Logs(control::Logs { lines, next: self.next_log })
            }
            Request::Stop => {
                self.stop_requested = true;
                Reply::Stopping
            }
        }
    }
}

/// Runs the dataflow at `path` until every node has exited. Returns whether
/// all of them succeeded.
pub fn run(path: &Path) -> io::Result<bool> {
    let dataflow = Dataflow::load(path)?;
    let Graph { routes, upstream } = dataflow.resolve()?;
    let base_dir = path.parent().unwrap_or(Path::new("."));

    let files = RuntimeFiles::create()?;
    let node_listener = UnixListener::bind(files.nodes_socket())?;
    let control_listener = UnixListener::bind(files.control_socket())?;
    signals::install();

    let routes = Arc::new(routes);
    let state = Arc::new(Mutex::new(State {
        dataflow: std::fs::canonicalize(path)?,
        start: Instant::now(),
        routes: routes.clone(),
        expected: dataflow.nodes.iter().map(|n| n.id.clone()).collect(),
        writers: HashMap::new(),
        upstream,
        nodes: (dataflow.nodes.iter())
            .map(|n| {
                (
                    n.id.clone(),
                    NodeInfo {
                        pid: None,
                        state: NodeState::Starting,
                        regions: Vec::new(),
                        stop_sent: None,
                        terminated: false,
                    },
                )
            })
            .collect(),
        counters: routes.keys().map(|k| (k.clone(), (0, 0))).collect(),
        logs: VecDeque::new(),
        next_log: 0,
        stop_requested: false,
    }));
    {
        let (state, shm_dir) = (state.clone(), files.shm_dir.clone());
        std::thread::spawn(move || {
            for stream in node_listener.incoming().flatten() {
                let (state, routes, shm_dir) = (state.clone(), routes.clone(), shm_dir.clone());
                std::thread::spawn(move || handle_node(stream, &state, &routes, &shm_dir));
            }
        });
    }
    {
        let state = state.clone();
        control::serve(control_listener, move |request| state.lock().unwrap().handle(request));
    }
    log(&state, "daemon", format!("pid {}, control socket {}", std::process::id(), files.control_socket().display()));

    let mut children = Vec::new();
    let mut output_threads = Vec::new();
    for node in &dataflow.nodes {
        match spawn(node, base_dir, &files, &state) {
            Ok((child, threads)) => {
                children.push((node.id.clone(), child));
                output_threads.extend(threads);
            }
            Err(e) => {
                kill_all(&mut children, &state);
                return Err(e);
            }
        }
    }

    let ok = supervise(children, &state);
    for thread in output_threads {
        let _ = thread.join();
    }
    Ok(ok)
}

fn log(state: &Mutex<State>, node: &str, text: String) {
    state.lock().unwrap().log(node, text);
}

/// Starts a node in its own process group, with its output captured.
fn spawn(
    node: &NodeConfig,
    base_dir: &Path,
    files: &RuntimeFiles,
    state: &Arc<Mutex<State>>,
) -> io::Result<(Child, [JoinHandle<()>; 2])> {
    let exe = base_dir.join(&node.path);
    let mut command = Command::new(&exe);
    command
        .env(ENV_NODE_ID, &node.id)
        .env(ENV_DAEMON_SOCKET, files.nodes_socket())
        .env(ENV_SHM_DIR, &files.shm_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Keeps Ctrl-C in the terminal from reaching nodes directly: the
        // daemon decides how they stop.
        .process_group(0);
    // SAFETY: prctl is async-signal-safe. The death signal fires when the
    // spawning thread exits, which is the main thread: it outlives all nodes.
    unsafe {
        command.pre_exec(|| match libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) {
            -1 => Err(io::Error::last_os_error()),
            _ => Ok(()),
        });
    }
    let mut child = command
        .spawn()
        .map_err(|e| io::Error::other(format!("failed to spawn `{}` ({}): {e}", node.id, exe.display())))?;
    state.lock().unwrap().nodes.get_mut(&node.id).unwrap().pid = Some(child.id());

    let capture = |pipe: Box<dyn Read + Send>| {
        let (state, id) = (state.clone(), node.id.clone());
        std::thread::spawn(move || {
            for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                log(&state, &id, line);
            }
        })
    };
    let threads = [capture(Box::new(child.stdout.take().unwrap())), capture(Box::new(child.stderr.take().unwrap()))];
    Ok((child, threads))
}

fn handle_node(mut stream: UnixStream, state: &Mutex<State>, routes: &Routes, shm_dir: &Path) {
    let node_id = match NodeMsg::read_from(&mut stream) {
        Ok(Some(NodeMsg::Register { node_id })) => node_id,
        other => {
            log(state, "daemon", format!("expected Register as first message, got {other:?}"));
            return;
        }
    };
    {
        let mut s = state.lock().unwrap();
        if !s.expected.contains(&node_id) || s.writers.contains_key(&node_id) {
            s.log("daemon", format!("rejecting unknown or duplicate node `{node_id}`"));
            return;
        }
        let Ok(writer) = stream.try_clone() else { return };
        s.writers.insert(node_id.clone(), Arc::new(Mutex::new(writer)));
        if s.writers.len() == s.expected.len() {
            let count = s.expected.len();
            s.log("daemon", format!("all {count} nodes registered"));
            let ids: Vec<String> = s.writers.keys().cloned().collect();
            for id in ids {
                let _ = DaemonMsg::Ready.write_to(&mut *s.writers[&id].lock().unwrap());
                s.set_state(&id, NodeState::Running);
            }
        }
    }

    // This node's regions, mapped only to adjust their reference counts.
    let mut regions: HashMap<u32, Arc<Region>> = HashMap::new();
    loop {
        match NodeMsg::read_from(&mut stream) {
            Ok(Some(NodeMsg::Output { output_id, slot, len })) => {
                let region = match regions.get(&slot) {
                    Some(region) => region.clone(),
                    None => match Region::open(&shm::region_path(shm_dir, &node_id, slot)) {
                        Ok(region) => {
                            let region = Arc::new(region);
                            regions.insert(slot, region.clone());
                            let mut s = state.lock().unwrap();
                            s.nodes.get_mut(&node_id).unwrap().regions.push(region.clone());
                            region
                        }
                        Err(err) => {
                            log(
                                state,
                                "daemon",
                                format!("`{node_id}` sent from region {slot}, which can't be opened: {err}"),
                            );
                            break;
                        }
                    },
                };
                let key = (node_id.clone(), output_id);
                let targets: Vec<_> = {
                    let mut s = state.lock().unwrap();
                    let counter = s.counters.entry(key.clone()).or_default();
                    counter.0 += 1;
                    counter.1 += len;
                    (routes.get(&key).into_iter().flatten())
                        .filter_map(|(target, input_id)| Some((s.writers.get(target)?.clone(), input_id)))
                        .collect()
                };
                for (writer, input_id) in targets {
                    // Take the receiver's reference before it can see the message.
                    region.refcount().fetch_add(1, Ordering::Relaxed);
                    let msg = DaemonMsg::Input { input_id: input_id.clone(), source: node_id.clone(), slot, len };
                    if msg.write_to(&mut *writer.lock().unwrap()).is_err() {
                        // The target has exited; it will never release it.
                        region.refcount().fetch_sub(1, Ordering::Release);
                    }
                }
                // Drop the in-transit reference the sender took.
                region.refcount().fetch_sub(1, Ordering::Release);
            }
            Ok(None) => break,
            Ok(Some(other)) => {
                log(state, "daemon", format!("unexpected message from `{node_id}`: {other:?}"));
                break;
            }
            // The node died; `supervise` reports how.
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => break,
            Err(e) => {
                log(state, "daemon", format!("connection to `{node_id}` failed: {e}"));
                break;
            }
        }
    }
    node_disconnected(&node_id, state);
}

/// Removes `node_id` and sends `Stop` to nodes that have no upstream left.
fn node_disconnected(node_id: &str, state: &Mutex<State>) {
    let mut s = state.lock().unwrap();
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

/// Asks a node to stop, once. From then on it has `STOP_GRACE` to exit
/// before `supervise` escalates.
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

/// Waits for all nodes to exit. If one fails, kills the others.
///
/// Stopping drains the dataflow: sources get `Stop` first, and each node gets
/// it once all its upstream nodes have exited, so no in-flight message is
/// dropped. Nodes not reached that way (cycles) get it after `DRAIN_TIMEOUT`.
/// A node that ignores `Stop` gets SIGTERM, then SIGKILL.
fn supervise(mut children: Vec<(String, Child)>, state: &Mutex<State>) -> bool {
    let mut ok = true;
    let mut stop_started: Option<Instant> = None;
    let mut drained = false;
    while !children.is_empty() {
        let mut i = 0;
        while i < children.len() {
            match children[i].1.try_wait() {
                Ok(Some(status)) => {
                    let (id, _) = children.remove(i);
                    let mut s = state.lock().unwrap();
                    // A node we terminated did what it was asked.
                    let terminated = s.nodes[&id].terminated && status.signal() == Some(libc::SIGTERM);
                    let success = status.success() || terminated;
                    s.log("daemon", format!("`{id}` exited: {status}"));
                    s.set_state(&id, NodeState::Exited { success, detail: status.to_string() });
                    let stopping = s.stop_requested;
                    drop(s);
                    if !success {
                        ok = false;
                        if !stopping {
                            log(state, "daemon", format!("stopping the dataflow because `{id}` failed"));
                            kill_all(&mut children, state);
                        }
                    }
                }
                Ok(None) => i += 1,
                Err(e) => {
                    log(state, "daemon", format!("failed to wait for `{}`: {e}", children[i].0));
                    children.remove(i);
                    ok = false;
                }
            }
        }

        let signals = signals::received();
        if signals >= 2 {
            log(state, "daemon", "interrupted again, killing all nodes".into());
            kill_all(&mut children, state);
            ok = false;
        }
        let mut s = state.lock().unwrap();
        s.stop_requested |= signals == 1;
        if s.stop_requested && stop_started.is_none() {
            stop_started = Some(Instant::now());
            s.log("daemon", "stopping the dataflow, starting with its sources".into());
            let sources: Vec<String> =
                (s.upstream.iter()).filter(|(_, up)| up.is_empty()).map(|(id, _)| id.clone()).collect();
            for id in sources {
                send_stop(&mut s, &id);
            }
        }
        if !drained && stop_started.is_some_and(|t| t.elapsed() > DRAIN_TIMEOUT) {
            drained = true;
            let ids: Vec<String> = s.nodes.keys().cloned().collect();
            if ids
                .iter()
                .any(|id| s.nodes[id].stop_sent.is_none() && !matches!(s.nodes[id].state, NodeState::Exited { .. }))
            {
                s.log("daemon", format!("dataflow not drained after {DRAIN_TIMEOUT:?}, stopping the remaining nodes"));
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

fn kill_all(children: &mut Vec<(String, Child)>, state: &Mutex<State>) {
    for (id, child) in children.iter_mut() {
        let _ = child.kill();
        let _ = child.wait();
        let mut s = state.lock().unwrap();
        s.log("daemon", format!("killed `{id}`"));
        s.set_state(id, NodeState::Exited { success: false, detail: "killed".into() });
    }
    children.clear();
}
