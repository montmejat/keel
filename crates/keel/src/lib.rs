//! keel node API.
//!
//! A node is a process spawned by the daemon. It connects back over the Unix
//! socket named in its environment, publishes on its outputs and receives
//! events on its inputs. Payloads travel through shared memory (see [`shm`]);
//! the socket only carries small descriptors.
//!
//! Every message is traced (see [`trace`]): an output sent while the node
//! holds an input sample belongs to that input's trace, so a chain of
//! messages can be followed through every node it passes.

pub mod protocol;
pub mod shm;
pub mod trace;

use std::collections::HashMap;
use std::io;
use std::ops::Deref;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};

use protocol::{DaemonMsg, NodeMsg, ENV_DAEMON_SOCKET, ENV_NODE_ID, ENV_SHM_DIR};
use shm::{Pool, Region};
use trace::{Context, EventWriter, RawEvent, StatsWriter, ENV_NODE_INDEX};

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
pub struct Sample(Arc<Held>);

impl Sample {
    /// The message's trace context.
    pub fn context(&self) -> &Context {
        &self.0.context
    }
}

impl Deref for Sample {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        // SAFETY: we hold a reference until drop, and `len` was checked
        // against the mapping's capacity on receipt.
        unsafe { self.0.region.payload_slice(self.0.len) }
    }
}

/// A reference to a received region. Releasing it ends the message's
/// processing, for the stats and the trace.
struct Held {
    region: Arc<Region>,
    len: usize,
    context: Context,
    /// Input slot in the stats file, and when the message was taken.
    input: Option<usize>,
    taken_ns: u64,
    tracer: Arc<Tracer>,
}

impl Drop for Held {
    fn drop(&mut self) {
        if let Some(slot) = self.input {
            let now = trace::now_ns();
            self.tracer.stats.record_processing(slot, now.saturating_sub(self.taken_ns));
            if self.context.sampled {
                self.tracer.event(trace::RELEASED, &self.context, now, slot as u64);
            }
        }
        self.region.refcount().fetch_sub(1, Ordering::Release);
    }
}

/// This node's tracing files. Shared with samples, which record their
/// release from wherever they're dropped.
struct Tracer {
    stats: StatsWriter,
    events: EventWriter,
}

impl Tracer {
    fn event(&self, kind: u64, context: &Context, t_ns: u64, aux: u64) {
        let (span, trace, parent) = (context.span, context.trace, context.parent);
        self.events.push(RawEvent { kind, span, trace, parent, t_ns, aux });
    }
}

pub struct Node {
    id: String,
    index: u16,
    stream: UnixStream,
    shm_dir: PathBuf,
    pool: Pool,
    /// Mappings of other nodes' regions, by `(source, slot)`.
    inputs: HashMap<(String, u32), Arc<Region>>,
    tracer: Arc<Tracer>,
    /// Messages sent so far; the next one's sequence number.
    sent: u64,
    /// The latest input received, while the node still holds it: what the
    /// next output is caused by.
    last_input: Weak<Held>,
    last_sampled_root_ns: u64,
}

impl Node {
    /// Connects to the daemon that spawned this process. Blocks until every
    /// node in the dataflow has registered.
    pub fn from_env() -> io::Result<Self> {
        let id = env(ENV_NODE_ID)?;
        let index = env(ENV_NODE_INDEX)?.parse().map_err(|_| io::Error::other(format!("invalid {ENV_NODE_INDEX}")))?;
        let shm_dir = PathBuf::from(env(ENV_SHM_DIR)?);
        let tracer = Arc::new(Tracer {
            stats: StatsWriter::create(&shm_dir, &id)?,
            events: EventWriter::create(&shm_dir, &id)?,
        });
        let mut stream = UnixStream::connect(env(ENV_DAEMON_SOCKET)?)?;
        NodeMsg::Register { node_id: id.clone() }.write_to(&mut stream)?;
        match DaemonMsg::read_from(&mut stream)? {
            Some(DaemonMsg::Ready) => {}
            other => return Err(io::Error::other(format!("expected Ready from daemon, got {other:?}"))),
        }
        let pool = Pool::new(shm_dir.clone(), id.clone());
        Ok(Self {
            id,
            index,
            stream,
            shm_dir,
            pool,
            inputs: HashMap::new(),
            tracer,
            sent: 0,
            last_input: Weak::new(),
            last_sampled_root_ns: 0,
        })
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
    ///
    /// The message continues the trace of the latest input the node still
    /// holds, if any; otherwise it starts a new trace.
    pub fn send_with(&mut self, output_id: &str, len: usize, fill: impl FnOnce(&mut [u8])) -> io::Result<()> {
        let cause = self.last_input.upgrade().map(|held| held.context);
        self.send_traced(output_id, len, cause, fill)
    }

    /// Like [`Node::send_with`], naming the input this message results from,
    /// for nodes that combine several inputs.
    pub fn send_caused_by(
        &mut self,
        output_id: &str,
        cause: &Sample,
        len: usize,
        fill: impl FnOnce(&mut [u8]),
    ) -> io::Result<()> {
        self.send_traced(output_id, len, Some(cause.0.context), fill)
    }

    fn send_traced(
        &mut self,
        output_id: &str,
        len: usize,
        cause: Option<Context>,
        fill: impl FnOnce(&mut [u8]),
    ) -> io::Result<()> {
        let slot = self.pool.acquire(len)?;
        fill(self.pool.payload_mut(slot, len));

        let span = trace::span_id(self.index, self.sent);
        self.sent += 1;
        let now = trace::now_ns();
        let context = match cause {
            Some(cause) => {
                Context { span, trace: cause.trace, parent: cause.span, published_ns: now, sampled: cause.sampled }
            }
            None => {
                let sampled = now.saturating_sub(self.last_sampled_root_ns) >= trace::SAMPLE_INTERVAL_NS;
                if sampled {
                    self.last_sampled_root_ns = now;
                }
                Context { span, trace: span, parent: 0, published_ns: now, sampled }
            }
        };
        self.pool.region(slot).set_context(&context);
        self.pool.publish(slot);
        if context.sampled {
            self.tracer.event(trace::PUBLISHED, &context, now, 0);
        }
        NodeMsg::Output { output_id: output_id.to_owned(), slot, len: len as u64 }.write_to(&mut self.stream)
    }

    /// Blocks until the next event. A closed connection is reported as `Stop`.
    pub fn next_event(&mut self) -> io::Result<Event> {
        match DaemonMsg::read_from(&mut self.stream)? {
            Some(DaemonMsg::Input { input_id, source, slot, len }) => {
                let data = self.sample(source, slot, len as usize, &input_id)?;
                self.last_input = Arc::downgrade(&data.0);
                Ok(Event::Input { id: input_id, data })
            }
            Some(DaemonMsg::Stop) | None => Ok(Event::Stop),
            Some(other) => Err(io::Error::other(format!("unexpected message from daemon: {other:?}"))),
        }
    }

    /// Wraps a reference the daemon handed us, mapping the region on first
    /// use and again whenever the sender has grown it.
    fn sample(&mut self, source: String, slot: u32, len: usize, input_id: &str) -> io::Result<Sample> {
        let key = (source, slot);
        let region = match self.inputs.get(&key) {
            Some(region) if region.capacity() >= len => region.clone(),
            _ => {
                let region = Arc::new(Region::open(&shm::region_path(&self.shm_dir, &key.0, slot))?);
                self.inputs.insert(key, region.clone());
                region
            }
        };
        if region.capacity() < len {
            region.refcount().fetch_sub(1, Ordering::Release);
            return Err(io::Error::new(io::ErrorKind::InvalidData, "payload is larger than its region"));
        }
        let taken_ns = trace::now_ns();
        let context = region.context();
        let input = self.tracer.stats.slot(input_id);
        if let Some(slot) = input {
            self.tracer.stats.record_latency(slot, taken_ns.saturating_sub(context.published_ns));
            if context.sampled {
                self.tracer.event(trace::TAKEN, &context, taken_ns, slot as u64);
            }
        }
        Ok(Sample(Arc::new(Held { region, len, context, input, taken_ns, tracer: self.tracer.clone() })))
    }
}

fn env(name: &str) -> io::Result<String> {
    std::env::var(name)
        .map_err(|_| io::Error::other(format!("{name} is not set; nodes must be started by keel-daemon")))
}
