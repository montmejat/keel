//! keel node API.
//!
//! A node is a process spawned by the daemon. It connects back over the Unix
//! socket named in its environment, publishes on its outputs and receives
//! events on its inputs. Payloads travel through shared memory (see [`shm`]);
//! the socket only carries small descriptors.

pub mod protocol;
pub mod shm;

use std::collections::HashMap;
use std::io;
use std::ops::Deref;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use protocol::{DaemonMsg, NodeMsg, ENV_DAEMON_SOCKET, ENV_NODE_ID, ENV_SHM_DIR};
use shm::{Pool, Region};

pub enum Event {
    Input {
        id: String,
        data: Sample,
    },
    /// All upstream nodes have exited; the node should return.
    Stop,
}

/// A received payload, read in place from the sender's shared memory. The
/// sender can reuse that memory once every receiver has dropped its sample.
pub struct Sample {
    region: Arc<Region>,
    len: usize,
}

impl Deref for Sample {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        // SAFETY: we hold a reference until drop, and `len` was checked
        // against the mapping's capacity on receipt.
        unsafe { self.region.payload_slice(self.len) }
    }
}

impl Drop for Sample {
    fn drop(&mut self) {
        self.region.refcount().fetch_sub(1, Ordering::Release);
    }
}

pub struct Node {
    id: String,
    stream: UnixStream,
    shm_dir: PathBuf,
    pool: Pool,
    /// Mappings of other nodes' regions, by `(source, slot)`.
    inputs: HashMap<(String, u32), Arc<Region>>,
}

impl Node {
    /// Connects to the daemon that spawned this process. Blocks until every
    /// node in the dataflow has registered.
    pub fn from_env() -> io::Result<Self> {
        let id = env(ENV_NODE_ID)?;
        let shm_dir = PathBuf::from(env(ENV_SHM_DIR)?);
        let mut stream = UnixStream::connect(env(ENV_DAEMON_SOCKET)?)?;
        NodeMsg::Register { node_id: id.clone() }.write_to(&mut stream)?;
        match DaemonMsg::read_from(&mut stream)? {
            Some(DaemonMsg::Ready) => {}
            other => return Err(io::Error::other(format!("expected Ready from daemon, got {other:?}"))),
        }
        let pool = Pool::new(shm_dir.clone(), id.clone());
        Ok(Self { id, stream, shm_dir, pool, inputs: HashMap::new() })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    /// Sends `data` on an output. Copies it once, into shared memory; use
    /// [`Node::send_with`] to write the payload in place instead.
    pub fn send_output(&mut self, output_id: &str, data: &[u8]) -> io::Result<()> {
        self.send_with(output_id, data.len(), |buf| buf.copy_from_slice(data))
    }

    /// Sends a `len`-byte payload that `fill` writes directly into shared
    /// memory. Waits if all of this node's regions are still being read.
    pub fn send_with(&mut self, output_id: &str, len: usize, fill: impl FnOnce(&mut [u8])) -> io::Result<()> {
        let slot = self.pool.acquire(len)?;
        fill(self.pool.payload_mut(slot, len));
        self.pool.publish(slot);
        NodeMsg::Output { output_id: output_id.to_owned(), slot, len: len as u64 }.write_to(&mut self.stream)
    }

    /// Blocks until the next event. A closed connection is reported as `Stop`.
    pub fn next_event(&mut self) -> io::Result<Event> {
        match DaemonMsg::read_from(&mut self.stream)? {
            Some(DaemonMsg::Input { input_id, source, slot, len }) => {
                let data = self.sample(source, slot, len as usize)?;
                Ok(Event::Input { id: input_id, data })
            }
            Some(DaemonMsg::Stop) | None => Ok(Event::Stop),
            Some(other) => Err(io::Error::other(format!("unexpected message from daemon: {other:?}"))),
        }
    }

    /// Wraps a reference the daemon handed us, mapping the region on first
    /// use and again whenever the sender has grown it.
    fn sample(&mut self, source: String, slot: u32, len: usize) -> io::Result<Sample> {
        let key = (source, slot);
        let region = match self.inputs.get(&key) {
            Some(region) if region.capacity() >= len => region.clone(),
            _ => {
                let region = Arc::new(Region::open(&shm::region_path(&self.shm_dir, &key.0, slot))?);
                self.inputs.insert(key, region.clone());
                region
            }
        };
        // Wrap before checking so that the reference is released either way.
        let mut sample = Sample { region, len: 0 };
        if sample.region.capacity() < len {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "payload is larger than its region"));
        }
        sample.len = len;
        Ok(sample)
    }
}

fn env(name: &str) -> io::Result<String> {
    std::env::var(name)
        .map_err(|_| io::Error::other(format!("{name} is not set; nodes must be started by keel-daemon")))
}
