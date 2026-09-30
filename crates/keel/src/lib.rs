//! keel node API.
//!
//! A node is a process spawned by the daemon. It registers over the Unix
//! socket named in its environment and gets its routes back. From then on,
//! messages don't involve the daemon: payloads are written in place in shared
//! memory (see [`shm`]), and descriptors go straight into each receiver's
//! channel, which wakes the receiver with a futex (see [`channel`]). Nothing
//! is allocated per message.
//!
//! Every message is traced (see [`trace`]): an output sent while the node
//! holds an input sample belongs to that input's trace, so a chain of
//! messages can be followed through every node it passes.

pub mod channel;
pub mod periodic;
pub mod protocol;
pub mod shm;
pub mod trace;

use std::collections::HashMap;
use std::io;
use std::ops::Deref;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use channel::{Bell, Channel, Target, DAEMON};
pub use periodic::Periodic;
use protocol::{DaemonMsg, NodeMsg, ENV_DAEMON_SOCKET, ENV_NODE_ID, ENV_REALTIME, ENV_SHM_DIR};
use shm::{Pool, Region};
use trace::{Context, EventWriter, RawEvent, StatsWriter, ENV_NODE_INDEX};

pub enum Event {
    Input {
        id: &'static str,
        data: Sample,
    },
    /// All upstream nodes have exited and every message they sent has been
    /// received; the node should return.
    Stop,
}

/// A received payload, read in place from the sender's shared memory. The
/// sender can reuse that memory once every receiver has dropped its sample.
/// Dropping it also ends the message's processing, for the stats and traces.
pub struct Sample {
    region: Arc<Region>,
    len: usize,
    context: Context,
    /// The input's slot in the stats file.
    input: usize,
    taken_ns: u64,
    tracer: Arc<Tracer>,
}

impl Sample {
    /// The message's trace context.
    pub fn context(&self) -> &Context {
        &self.context
    }
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
        let now = trace::now_ns();
        self.tracer.stats.record_processing(self.input, now.saturating_sub(self.taken_ns));
        if self.context.sampled {
            self.tracer.event(trace::RELEASED, &self.context, now, self.input as u64);
        }
        let span = self.context.span;
        let _ = self.tracer.holding.compare_exchange(span, 0, Ordering::Relaxed, Ordering::Relaxed);
        self.region.refcount().fetch_sub(1, Ordering::Release);
    }
}

/// This node's tracing files, shared with its samples, which record their
/// release from wherever they're dropped.
struct Tracer {
    stats: StatsWriter,
    events: EventWriter,
    /// Span of the latest input received, while its sample is alive: what the
    /// next output is caused by.
    holding: AtomicU64,
}

impl Tracer {
    fn event(&self, kind: u64, context: &Context, t_ns: u64, aux: u64) {
        let (span, trace, parent) = (context.span, context.trace, context.parent);
        self.events.push(RawEvent { kind, span, trace, parent, t_ns, aux });
    }
}

struct Input {
    id: &'static str,
    source: String,
    channel: Channel,
    /// The sender's regions this node has mapped, by slot.
    regions: Vec<Option<Arc<Region>>>,
}

struct Output {
    stats_slot: Option<usize>,
    targets: Vec<Target>,
}

pub struct Node {
    id: String,
    index: u16,
    /// Only kept open, so that the daemon sees when this process exits.
    _socket: UnixStream,
    shm_dir: PathBuf,
    pool: Pool,
    inputs: Vec<Input>,
    outputs: HashMap<String, Output>,
    bell: Bell,
    /// Inputs are read round-robin, starting here.
    next_input: usize,
    tracer: Arc<Tracer>,
    /// Messages sent so far; the next one's sequence number.
    sent: u64,
    last_input: Context,
    last_sampled_root_ns: u64,
}

impl Node {
    /// Connects to the daemon that spawned this process. Blocks until every
    /// node in the dataflow has registered.
    pub fn from_env() -> io::Result<Self> {
        let id = env(ENV_NODE_ID)?;
        let index = env(ENV_NODE_INDEX)?.parse().map_err(|_| io::Error::other(format!("invalid {ENV_NODE_INDEX}")))?;
        let shm_dir = PathBuf::from(env(ENV_SHM_DIR)?);
        if std::env::var_os(ENV_REALTIME).is_some() {
            realtime_setup();
        }
        let tracer = Arc::new(Tracer {
            stats: StatsWriter::create(&shm_dir, &id)?,
            events: EventWriter::create(&shm_dir, &id)?,
            holding: AtomicU64::new(0),
        });
        let mut socket = UnixStream::connect(env(ENV_DAEMON_SOCKET)?)?;
        NodeMsg::Register { node_id: id.clone() }.write_to(&mut socket)?;
        let routes = match DaemonMsg::read_from(&mut socket)? {
            Some(DaemonMsg::Ready { routes }) => routes,
            other => return Err(io::Error::other(format!("expected Ready from daemon, got {other:?}"))),
        };

        let mut inputs = Vec::new();
        let mut outputs: HashMap<String, Output> = HashMap::new();
        let mut bells: HashMap<String, Arc<Bell>> = HashMap::new();
        let mut bell = |owner: &str| -> io::Result<Arc<Bell>> {
            if let Some(bell) = bells.get(owner) {
                return Ok(bell.clone());
            }
            let bell = Arc::new(Bell::open(&channel::bell_path(&shm_dir, owner))?);
            bells.insert(owner.to_owned(), bell.clone());
            Ok(bell)
        };
        for line in routes.lines() {
            match line.split_whitespace().collect::<Vec<_>>()[..] {
                ["in", input, source] => {
                    tracer.stats.slot(input);
                    inputs.push(Input {
                        // Input names live as long as the node; leaking them
                        // once spares an allocation per message.
                        id: Box::leak(input.to_owned().into_boxed_str()),
                        source: source.to_owned(),
                        channel: Channel::open(&channel::channel_path(&shm_dir, &id, input))?,
                        regions: Vec::new(),
                    });
                }
                ["out", output, target, input] => {
                    let target = Target {
                        channel: Channel::open(&channel::channel_path(&shm_dir, target, input))?,
                        bell: bell(target)?,
                    };
                    let stats_slot = tracer.stats.output_slot(output);
                    outputs
                        .entry(output.to_owned())
                        .or_insert(Output { stats_slot, targets: Vec::new() })
                        .targets
                        .push(target);
                }
                ["out", output, DAEMON] => {
                    let target = Target {
                        channel: Channel::open(&channel::forward_path(&shm_dir, &id, output))?,
                        bell: bell(DAEMON)?,
                    };
                    let stats_slot = tracer.stats.output_slot(output);
                    outputs
                        .entry(output.to_owned())
                        .or_insert(Output { stats_slot, targets: Vec::new() })
                        .targets
                        .push(target);
                }
                _ => return Err(io::Error::other(format!("invalid route from the daemon: {line}"))),
            }
        }
        let bell = Bell::open(&channel::bell_path(&shm_dir, &id))?;
        let pool = Pool::new(shm_dir.clone(), id.clone());
        Ok(Self {
            id,
            index,
            _socket: socket,
            shm_dir,
            pool,
            inputs,
            outputs,
            bell,
            next_input: 0,
            tracer,
            sent: 0,
            last_input: Context::default(),
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
        let holding = self.tracer.holding.load(Ordering::Relaxed);
        let cause = (holding != 0 && holding == self.last_input.span).then_some(self.last_input);
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
        self.send_traced(output_id, len, Some(cause.context), fill)
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
        if !self.outputs.contains_key(output_id) {
            // Nobody listens, but it still counts; allocates only this once.
            let stats_slot = self.tracer.stats.output_slot(output_id);
            self.outputs.insert(output_id.to_owned(), Output { stats_slot, targets: Vec::new() });
        }
        let output = &self.outputs[output_id];
        if context.sampled {
            let aux = output.stats_slot.map_or(u64::MAX, |s| s as u64);
            self.tracer.event(trace::PUBLISHED, &context, now, aux);
        }
        if let Some(stats_slot) = output.stats_slot {
            self.tracer.stats.record_output(stats_slot, len as u64);
        }
        let pool = &self.pool;
        channel::send(&output.targets, slot, len as u64, |s| pool.region(s));
        Ok(())
    }

    /// Blocks until the next event.
    pub fn next_event(&mut self) -> io::Result<Event> {
        loop {
            let seen = self.bell.seq();
            // Read before polling: once stop is set, every message sent by
            // upstream nodes is already in our channels.
            let stop = self.bell.stop_requested();
            if let Some(event) = self.poll()? {
                return Ok(event);
            }
            if stop {
                return Ok(Event::Stop);
            }
            self.bell.wait(seen, None);
        }
    }

    /// The next event if there is one, without waiting: for loops that must
    /// keep their own pace, like a periodic controller reading its sensors.
    pub fn try_next_event(&mut self) -> io::Result<Option<Event>> {
        let stop = self.bell.stop_requested();
        match self.poll()? {
            Some(event) => Ok(Some(event)),
            None => Ok(stop.then_some(Event::Stop)),
        }
    }

    /// Takes a message from the next input that has one, round-robin.
    fn poll(&mut self) -> io::Result<Option<Event>> {
        let n = self.inputs.len();
        for k in 0..n {
            let i = (self.next_input + k) % n;
            if let Some((slot, len)) = self.inputs[i].channel.pop() {
                self.next_input = i + 1;
                return self.take(i, slot, len as usize).map(Some);
            }
        }
        Ok(None)
    }

    /// Wraps a reference the sender handed us, mapping the region on first
    /// use and again whenever the sender has grown it.
    fn take(&mut self, i: usize, slot: u32, len: usize) -> io::Result<Event> {
        let input = &mut self.inputs[i];
        let slot = slot as usize;
        if input.regions.len() <= slot {
            input.regions.resize(slot + 1, None);
        }
        let region = match &input.regions[slot] {
            Some(region) if region.capacity() >= len => region.clone(),
            _ => {
                let region = Arc::new(Region::open(&shm::region_path(&self.shm_dir, &input.source, slot as u32))?);
                input.regions[slot] = Some(region.clone());
                region
            }
        };
        if region.capacity() < len {
            region.refcount().fetch_sub(1, Ordering::Release);
            return Err(io::Error::new(io::ErrorKind::InvalidData, "payload is larger than its region"));
        }
        let taken_ns = trace::now_ns();
        let context = region.context();
        self.tracer.stats.record_latency(i, taken_ns.saturating_sub(context.published_ns));
        if context.sampled {
            self.tracer.event(trace::TAKEN, &context, taken_ns, i as u64);
        }
        self.tracer.holding.store(context.span, Ordering::Relaxed);
        self.last_input = context;
        let data = Sample { region, len, context, input: i, taken_ns, tracer: self.tracer.clone() };
        Ok(Event::Input { id: input.id, data })
    }
}

/// For nodes the dataflow marks real-time: wake-ups on time, and no page
/// faults. Scheduling priority and CPUs are set by the daemon.
fn realtime_setup() {
    // SAFETY: plain syscalls on this process.
    unsafe {
        // The default 50 µs of slack lets the kernel batch wake-ups.
        libc::prctl(libc::PR_SET_TIMERSLACK, 1);
        let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        libc::getrlimit(libc::RLIMIT_MEMLOCK, &mut limit);
        // Locking future mappings under a limit would make big regions fail
        // to map; lock only what's there then.
        let flags = if limit.rlim_cur == libc::RLIM_INFINITY {
            libc::MCL_CURRENT | libc::MCL_FUTURE
        } else {
            libc::MCL_CURRENT
        };
        if libc::mlockall(flags) != 0 {
            eprintln!("keel: can't lock memory: {}", io::Error::last_os_error());
        } else if flags == libc::MCL_CURRENT {
            eprintln!(
                "keel: memory locked, but not future allocations: the memory lock limit is {} KiB (ulimit -l)",
                limit.rlim_cur / 1024
            );
        }
    }
}

fn env(name: &str) -> io::Result<String> {
    std::env::var(name)
        .map_err(|_| io::Error::other(format!("{name} is not set; nodes must be started by a keel daemon")))
}
