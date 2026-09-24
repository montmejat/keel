//! keel-daemon: runs one dataflow on this machine.
//!
//! Spawns every node, waits for all of them to register, then routes each
//! output to the inputs subscribed to it. A node gets `Stop` once all of its
//! upstream nodes have exited. The daemon exits when all nodes have.

pub mod dataflow;

use std::collections::{HashMap, HashSet};
use std::io;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::process::{Child, Command};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dataflow::{Dataflow, Graph, Routes};
use keel::protocol::{DaemonMsg, NodeMsg, ENV_DAEMON_SOCKET, ENV_NODE_ID};

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

    let socket_path = std::env::temp_dir().join(format!("keel-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&socket_path);
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
                let (state, routes) = (state.clone(), routes.clone());
                std::thread::spawn(move || handle_node(stream, &state, &routes));
            }
        });
    }

    let mut children: Vec<(String, Child)> = Vec::new();
    for node in &dataflow.nodes {
        let exe = base_dir.join(&node.path);
        let child = Command::new(&exe)
            .env(ENV_NODE_ID, &node.id)
            .env(ENV_DAEMON_SOCKET, &socket_path)
            .spawn()
            .map_err(|e| io::Error::other(format!("failed to spawn `{}` ({}): {e}", node.id, exe.display())));
        match child {
            Ok(child) => children.push((node.id.clone(), child)),
            Err(e) => {
                kill_all(&mut children);
                let _ = std::fs::remove_file(&socket_path);
                return Err(e);
            }
        }
    }

    let ok = supervise(children);
    let _ = std::fs::remove_file(&socket_path);
    Ok(ok)
}

fn handle_node(mut stream: UnixStream, state: &Mutex<State>, routes: &Routes) {
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
        let Ok(writer) = stream.try_clone() else { return };
        s.writers.insert(node_id.clone(), Arc::new(Mutex::new(writer)));
        if s.writers.len() == s.expected.len() {
            eprintln!("[daemon] all {} nodes registered", s.expected.len());
            for w in s.writers.values() {
                let _ = DaemonMsg::Ready.write_to(&mut *w.lock().unwrap());
            }
        }
    }

    loop {
        match NodeMsg::read_from(&mut stream) {
            Ok(Some(NodeMsg::Output { output_id, data })) => {
                let Some(targets) = routes.get(&(node_id.clone(), output_id)) else { continue };
                for (target, input_id) in targets {
                    let writer = state.lock().unwrap().writers.get(target).cloned();
                    if let Some(w) = writer {
                        let msg = DaemonMsg::Input { input_id: input_id.clone(), data: data.clone() };
                        // The target may have exited already; that is not our problem.
                        let _ = msg.write_to(&mut *w.lock().unwrap());
                    }
                }
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
