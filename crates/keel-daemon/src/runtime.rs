//! Where a daemon keeps its runtime files, and how tools find running daemons.
//!
//! ```text
//! $XDG_RUNTIME_DIR/keel/<pid>/nodes.sock     node connections
//! $XDG_RUNTIME_DIR/keel/<pid>/control.sock   control API, see `control`
//! /dev/shm/keel-<pid>/<node>.<slot>          shared-memory regions
//! ```
//!
//! The sockets and regions of a dataflow go away when it ends; the control
//! socket, when the daemon exits. On startup, a daemon removes the files of
//! daemons that died without cleaning up.

use std::io;
use std::path::{Path, PathBuf};

/// `$XDG_RUNTIME_DIR/keel`, or a per-user directory in the temp dir.
pub fn base_dir() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) => PathBuf::from(dir).join("keel"),
        // SAFETY: getuid cannot fail.
        None => std::env::temp_dir().join(format!("keel-{}", unsafe { libc::getuid() })),
    }
}

pub fn control_socket(pid: u32) -> PathBuf {
    base_dir().join(pid.to_string()).join("control.sock")
}

/// Pids of the daemons currently running, in ascending order.
pub fn running_daemons() -> Vec<u32> {
    let Ok(entries) = std::fs::read_dir(base_dir()) else { return Vec::new() };
    let mut pids: Vec<u32> = entries
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse().ok())
        .filter(|&pid| alive(pid) && control_socket(pid).exists())
        .collect();
    pids.sort();
    pids
}

fn alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// The daemon's own directory and control socket, removed on drop.
pub(crate) struct RuntimeDir {
    pub dir: PathBuf,
}

impl RuntimeDir {
    pub fn create() -> io::Result<Self> {
        let base = base_dir();
        std::fs::create_dir_all(&base)?;
        remove_stale(&base, "");
        if Path::new(DEV_SHM).is_dir() {
            remove_stale(Path::new(DEV_SHM), "keel-");
        }
        let dir = base.join(std::process::id().to_string());
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir)?;
        Ok(Self { dir })
    }

    pub fn control_socket(&self) -> PathBuf {
        self.dir.join("control.sock")
    }
}

impl Drop for RuntimeDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

const DEV_SHM: &str = "/dev/shm";

/// The files of the dataflow a daemon is running: the socket its nodes
/// connect to and their shared-memory regions. Removed on drop.
pub(crate) struct SessionFiles {
    pub nodes_socket: PathBuf,
    pub shm_dir: PathBuf,
}

impl SessionFiles {
    /// `runtime_dir` is the daemon's [`RuntimeDir::dir`].
    pub fn create(runtime_dir: &Path) -> io::Result<Self> {
        // tmpfs, so regions live in RAM. Without it, fall back to our own dir.
        let shm_dir = match Path::new(DEV_SHM) {
            dev_shm if dev_shm.is_dir() => dev_shm.join(format!("keel-{}", std::process::id())),
            _ => runtime_dir.join("shm"),
        };
        let files = Self { nodes_socket: runtime_dir.join("nodes.sock"), shm_dir };
        let _ = std::fs::remove_file(&files.nodes_socket);
        let _ = std::fs::remove_dir_all(&files.shm_dir);
        std::fs::create_dir(&files.shm_dir)?;
        Ok(files)
    }
}

impl Drop for SessionFiles {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.nodes_socket);
        let _ = std::fs::remove_dir_all(&self.shm_dir);
    }
}

/// Removes `<prefix><pid>` entries in `dir` whose daemon is gone.
fn remove_stale(dir: &Path, prefix: &str) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|n| n.strip_prefix(prefix)?.parse::<u32>().ok()) else { continue };
        if !alive(pid) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}
