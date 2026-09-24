//! keel-daemon: runs one dataflow on this machine.
//!
//! Spawns every node, waits for all of them to register, then routes each
//! output to the inputs subscribed to it. A node gets `Stop` once all of its
//! upstream nodes have exited. The daemon exits when all nodes have.
//!
//! Payloads never pass through the daemon: nodes exchange shared-memory
//! regions, and the daemon forwards descriptors and keeps the regions'
//! reference counts right (see `keel::shm`).

pub mod dataflow;

use std::collections::{HashMap, HashSet};
use std::io;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dataflow::{Dataflow, Graph, Routes};
use keel::protocol::{DaemonMsg, NodeMsg, ENV_DAEMON_SOCKET, ENV_NODE_ID, ENV_SHM_DIR};
use keel::shm::{self, Region};

struct State {
    expected: HashSet<String>,
    writers: HashMap<String, Arc<Mutex<UnixStream>>>,
    /// Nodes still running upstream of each node.
    upstream: HashMap<String, HashSet<String>>,
}

/// Runs the dataflow at `path` until every node has exited. Returns whether
/// all of them succeeded.
pub fn run(path: &Path) -> io::Result<bool> {
    let dataflow = Dataflow::load(path)?;
    let Graph { routes, upstream } = dataflow.resolve()?;
    let base_dir = path.parent().unwrap_or(Path::new("."));

    let files = RuntimeFiles::create()?;
    let socket_path = files.socket.clone();
    let shm_dir = Arc::new(files.shm_dir.clone());
    let listener = UnixListener::bind(&socket_path)?;

    let state = Arc::new(Mutex::new(State {
        expected: dataflow.nodes.iter().map(|n| n.id.clone()).collect(),
        writers: HashMap::new(),
        upstream,
    }));
    let routes = Arc::new(routes);
    {
        let state = state.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (state, routes, shm_dir) = (state.clone(), routes.clone(), shm_dir.clone());
                std::thread::spawn(move || handle_node(stream, &state, &routes, &shm_dir));
            }
        });
    }

    let mut children: Vec<(String, Child)> = Vec::new();
    for node in &dataflow.nodes {
        let exe = base_dir.join(&node.path);
        let child = Command::new(&exe)
            .env(ENV_NODE_ID, &node.id)
            .env(ENV_DAEMON_SOCKET, &socket_path)
            .env(ENV_SHM_DIR, &files.shm_dir)
            .spawn()
            .map_err(|e| io::Error::other(format!("failed to spawn `{}` ({}): {e}", node.id, exe.display())));
        match child {
            Ok(child) => children.push((node.id.clone(), child)),
            Err(e) => {
                kill_all(&mut children);
                return Err(e);
            }
        }
    }

    Ok(supervise(children))
}

/// The daemon's socket and shared-memory directory, removed on drop.
struct RuntimeFiles {
    socket: PathBuf,
    shm_dir: PathBuf,
}

impl RuntimeFiles {
    fn create() -> io::Result<Self> {
        let name = format!("keel-{}", std::process::id());
        // tmpfs, so regions live in RAM; fall back to the temp dir elsewhere.
        let shm_base = Path::new("/dev/shm");
        let shm_base = if shm_base.is_dir() { shm_base.to_owned() } else { std::env::temp_dir() };
        remove_stale(&shm_base, "");
        remove_stale(&std::env::temp_dir(), ".sock");
        let files = Self { socket: std::env::temp_dir().join(format!("{name}.sock")), shm_dir: shm_base.join(name) };
        let _ = std::fs::remove_file(&files.socket);
        let _ = std::fs::remove_dir_all(&files.shm_dir);
        std::fs::create_dir(&files.shm_dir)?;
        Ok(files)
    }
}

impl Drop for RuntimeFiles {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket);
        let _ = std::fs::remove_dir_all(&self.shm_dir);
    }
}

/// Removes `keel-<pid><suffix>` entries in `dir` left behind by daemons that
/// were killed before they could clean up.
fn remove_stale(dir: &Path, suffix: &str) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|n| n.strip_prefix("keel-")?.strip_suffix(suffix)?.parse::<u32>().ok())
        else {
            continue;
        };
        if !Path::new(&format!("/proc/{pid}")).exists() {
            let path = entry.path();
            let _ = if path.is_dir() { std::fs::remove_dir_all(&path) } else { std::fs::remove_file(&path) };
        }
    }
}

fn handle_node(mut stream: UnixStream, state: &Mutex<State>, routes: &Routes, shm_dir: &Path) {
    let node_id = match NodeMsg::read_from(&mut stream) {
        Ok(Some(NodeMsg::Register { node_id })) => node_id,
        other => {
            eprintln!("[daemon] expected Register as first message, got {other:?}");
            return;
        }
    };
    {
        let mut s = state.lock().unwrap();
        if !s.expected.contains(&node_id) || s.writers.contains_key(&node_id) {
            eprintln!("[daemon] rejecting unknown or duplicate node `{node_id}`");
            return;
        }
        let Ok(writer) = stream.try_clone() else {
            return;
        };
        s.writers.insert(node_id.clone(), Arc::new(Mutex::new(writer)));
        if s.writers.len() == s.expected.len() {
            eprintln!("[daemon] all {} nodes registered", s.expected.len());
            for w in s.writers.values() {
                let _ = DaemonMsg::Ready.write_to(&mut *w.lock().unwrap());
            }
        }
    }

    // This node's regions, mapped only to adjust their reference counts.
    let mut regions: HashMap<u32, Region> = HashMap::new();
    loop {
        match NodeMsg::read_from(&mut stream) {
            Ok(Some(NodeMsg::Output { output_id, slot, len })) => {
                let region = match regions.entry(slot) {
                    std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                    std::collections::hash_map::Entry::Vacant(e) => {
                        match Region::open(&shm::region_path(shm_dir, &node_id, slot)) {
                            Ok(region) => e.insert(region),
                            Err(err) => {
                                eprintln!("[daemon] `{node_id}` sent from region {slot}, which can't be opened: {err}");
                                break;
                            }
                        }
                    }
                };
                let targets = routes.get(&(node_id.clone(), output_id)).map_or(&[][..], |t| t);
                for (target, input_id) in targets {
                    let writer = state.lock().unwrap().writers.get(target).cloned();
                    let Some(w) = writer else { continue };
                    // Take the receiver's reference before it can see the message.
                    region.refcount().fetch_add(1, Ordering::Relaxed);
                    let msg = DaemonMsg::Input { input_id: input_id.clone(), source: node_id.clone(), slot, len };
                    if msg.write_to(&mut *w.lock().unwrap()).is_err() {
                        // The target has exited; it will never release it.
                        region.refcount().fetch_sub(1, Ordering::Release);
                    }
                }
                // Drop the in-transit reference the sender took.
                region.refcount().fetch_sub(1, Ordering::Release);
            }
            Ok(None) => break,
            Ok(Some(other)) => {
                eprintln!("[daemon] unexpected message from `{node_id}`: {other:?}");
                break;
            }
            Err(e) => {
                eprintln!("[daemon] connection to `{node_id}` failed: {e}");
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
        if let Some(w) = s.writers.get(&id) {
            let _ = DaemonMsg::Stop.write_to(&mut *w.lock().unwrap());
        }
    }
}

/// Waits for all nodes to exit. If one fails, stops the others.
fn supervise(mut children: Vec<(String, Child)>) -> bool {
    let mut ok = true;
    while !children.is_empty() {
        let mut i = 0;
        while i < children.len() {
            match children[i].1.try_wait() {
                Ok(Some(status)) => {
                    let (id, _) = children.remove(i);
                    eprintln!("[daemon] `{id}` exited: {status}");
                    if !status.success() {
                        ok = false;
                        kill_all(&mut children);
                    }
                }
                Ok(None) => i += 1,
                Err(e) => {
                    eprintln!("[daemon] failed to wait for `{}`: {e}", children[i].0);
                    children.remove(i);
                    ok = false;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    ok
}

fn kill_all(children: &mut Vec<(String, Child)>) {
    for (id, child) in children.iter_mut() {
        eprintln!("[daemon] killing `{id}`");
        let _ = child.kill();
        let _ = child.wait();
    }
    children.clear();
}
