//! Where a daemon keeps its runtime files, and how tools find running daemons.
//!
//! ```text
//! $XDG_RUNTIME_DIR/keel/<pid>/nodes.sock     node connections
//! $XDG_RUNTIME_DIR/keel/<pid>/control.sock   control API, see `control`
//! /dev/shm/keel-<pid>/<node>.<slot>          shared-memory regions
//! ```
//!
//! A daemon removes its files on exit, and on startup removes those of
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

/// This daemon's runtime files, removed on drop.
pub(crate) struct RuntimeFiles {
    pub dir: PathBuf,
    pub shm_dir: PathBuf,
}

impl RuntimeFiles {
    pub fn create() -> io::Result<Self> {
        let pid = std::process::id();
        let base = base_dir();
        std::fs::create_dir_all(&base)?;
        remove_stale(&base, "");
        let dir = base.join(pid.to_string());
        // tmpfs, so regions live in RAM. Without it, fall back to our own dir.
        let dev_shm = Path::new("/dev/shm");
        let shm_dir = if dev_shm.is_dir() {
            remove_stale(dev_shm, "keel-");
            dev_shm.join(format!("keel-{pid}"))
        } else {
            dir.join("shm")
        };
        let files = Self { dir, shm_dir };
        let _ = std::fs::remove_dir_all(&files.dir);
        let _ = std::fs::remove_dir_all(&files.shm_dir);
        std::fs::create_dir(&files.dir)?;
        std::fs::create_dir(&files.shm_dir)?;
        Ok(files)
    }

    pub fn nodes_socket(&self) -> PathBuf {
        self.dir.join("nodes.sock")
    }

    pub fn control_socket(&self) -> PathBuf {
        self.dir.join("control.sock")
    }
}

impl Drop for RuntimeFiles {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.shm_dir);
        let _ = std::fs::remove_dir_all(&self.dir);
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
