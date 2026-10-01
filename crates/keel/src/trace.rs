//! Tracing: what travels with each message, and what each process records.
//!
//! Every message carries a [`Context`] in its region header: its own span id,
//! the trace it belongs to (the span of the message that started the chain),
//! its parent (the message the sender was handling when it sent this one),
//! when it was published, and whether its trace is sampled.
//!
//! Two files per node sit next to its regions, written by the node and read
//! by the daemon:
//! - `<node>.stats`: for each input, histograms of latency (published →
//!   taken by the receiver) and processing (taken → released). Every message
//!   is counted; recording one is a clock read and a few atomic adds.
//! - `<node>.events`: a ring of hop events for sampled traces only. A source
//!   starts a sampled trace at most every [`SAMPLE_INTERVAL_NS`]; messages
//!   caused by a sampled one are sampled too. A full ring drops events and
//!   counts them, it never blocks the node.
//!
//! Times are `CLOCK_MONOTONIC` nanoseconds of the machine they were taken on.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Mutex;

use crate::shm::Mapping;

/// Set by the daemon: the node's position in the dataflow, part of its span ids.
pub const ENV_NODE_INDEX: &str = "KEEL_NODE_INDEX";

/// A source starts at most one sampled trace per this interval.
pub const SAMPLE_INTERVAL_NS: u64 = 100_000_000;

/// The trace context of one message.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Context {
    /// This message: `(node index + 1) << 48 | sequence number`.
    pub span: u64,
    /// The span that started the chain; equal to `span` for a root.
    pub trace: u64,
    /// The message being handled when this one was sent; 0 for a root.
    pub parent: u64,
    pub published_ns: u64,
    pub sampled: bool,
}

pub fn span_id(node_index: u16, seq: u64) -> u64 {
    ((node_index as u64 + 1) << 48) | (seq & SEQ_MASK)
}

/// The sequence number part of a span id: the message's number on its node.
pub fn span_seq(span: u64) -> u64 {
    span & SEQ_MASK
}

const SEQ_MASK: u64 = (1 << 48) - 1;

/// `CLOCK_MONOTONIC`, in nanoseconds. A vDSO call, no syscall.
pub fn now_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: writes into a local.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

pub fn stats_path(dir: &Path, node_id: &str) -> PathBuf {
    dir.join(format!("{node_id}.stats"))
}

pub fn events_path(dir: &Path, node_id: &str) -> PathBuf {
    dir.join(format!("{node_id}.events"))
}

// Histograms: log-linear buckets, 8 per power of two, so a percentile is
// known within 12.5%. Values below 8 ns get a bucket each; values past ~34 s
// land in the last one.

pub const BUCKETS: usize = 272;
const MAX_VALUE: u64 = (1 << 35) - 1;

pub fn bucket(ns: u64) -> usize {
    let ns = ns.min(MAX_VALUE);
    if ns < 8 {
        return ns as usize;
    }
    let e = 63 - ns.leading_zeros() as usize;
    8 * (e - 2) + ((ns >> (e - 3)) & 7) as usize
}

/// The middle of a bucket's range.
pub fn bucket_value(index: usize) -> u64 {
    if index < 8 {
        return index as u64;
    }
    let (e, sub) = (index / 8 + 2, (index % 8) as u64);
    let low = (8 + sub) << (e - 3);
    low + (1 << (e - 3)) / 2
}

/// Percentiles of a histogram, in nanoseconds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Summary {
    pub count: u64,
    pub p50: u64,
    pub p99: u64,
    pub p999: u64,
    pub max: u64,
}

impl Summary {
    pub fn of(buckets: &[u64], max: u64) -> Self {
        let count: u64 = buckets.iter().sum();
        let at = |q: f64| {
            let rank = ((count as f64 * q).ceil() as u64).max(1);
            let mut seen = 0;
            for (i, &n) in buckets.iter().enumerate() {
                seen += n;
                if seen >= rank {
                    return bucket_value(i).min(max);
                }
            }
            max
        };
        if count == 0 {
            return Self::default();
        }
        Self { count, p50: at(0.5), p99: at(0.99), p999: at(0.999), max }
    }
}

// The stats file: a header, one slot per input, then one per output.
//
//   [inputs: u64][outputs: u64][activity: u64][waiting: u64][padding to 64]
//   input:  [name_len: u64][name: 56 bytes][max_latency: u64][max_processing: u64]
//           [padding to 128][latency: BUCKETS × u64][processing: BUCKETS × u64]
//   output: [name_len: u64][name: 56 bytes][messages: u64][bytes: u64][padding to 128]

pub const MAX_INPUTS: usize = 32;
pub const MAX_OUTPUTS: usize = 32;
const NAME_LEN: usize = 56;
const SLOT_LEN: usize = 128 + 2 * BUCKETS * 8;
const OUTPUT_LEN: usize = 128;
const OUTPUTS_AT: usize = 64 + MAX_INPUTS * SLOT_LEN;
pub const STATS_LEN: usize = OUTPUTS_AT + MAX_OUTPUTS * OUTPUT_LEN;

fn slot_offset(slot: usize) -> usize {
    64 + slot * SLOT_LEN
}

fn output_offset(slot: usize) -> usize {
    OUTPUTS_AT + slot * OUTPUT_LEN
}

/// Writes a name at `base` (`[len: u64][bytes]`) before it's published.
fn write_name(map: &Mapping, base: usize, name: &str) {
    let name = &name.as_bytes()[..name.len().min(NAME_LEN)];
    // SAFETY: in bounds; readers only look at slots below the published count.
    unsafe { std::ptr::copy_nonoverlapping(name.as_ptr(), map.ptr().add(base + 8), name.len()) };
    map.u64_at(base).store(name.len() as u64, Ordering::Relaxed);
}

fn read_name(map: &Mapping, base: usize) -> String {
    let len = (map.u64_at(base).load(Ordering::Relaxed) as usize).min(NAME_LEN);
    // SAFETY: in bounds; written before the count that made it visible.
    let name = unsafe { std::slice::from_raw_parts(map.ptr().add(base + 8), len) };
    String::from_utf8_lossy(name).into_owned()
}

/// The node's side of its stats file.
pub struct StatsWriter {
    map: Mapping,
    slots: Mutex<HashMap<String, usize>>,
    outputs: Mutex<HashMap<String, usize>>,
}

impl StatsWriter {
    pub fn create(dir: &Path, node_id: &str) -> io::Result<Self> {
        Ok(Self {
            map: Mapping::create(&stats_path(dir, node_id), STATS_LEN)?,
            slots: Mutex::new(HashMap::new()),
            outputs: Mutex::new(HashMap::new()),
        })
    }

    /// The slot of `input`, registering it on first use. `None` once all
    /// slots are taken: that input then goes uncounted.
    pub fn slot(&self, input: &str) -> Option<usize> {
        let mut slots = self.slots.lock().unwrap();
        if let Some(&slot) = slots.get(input) {
            return Some(slot);
        }
        let slot = slots.len();
        if slot == MAX_INPUTS {
            return None;
        }
        write_name(&self.map, slot_offset(slot), input);
        self.map.u64_at(0).store(slot as u64 + 1, Ordering::Release);
        slots.insert(input.to_owned(), slot);
        Some(slot)
    }

    /// Like [`StatsWriter::slot`], for an output's counters.
    pub fn output_slot(&self, output: &str) -> Option<usize> {
        let mut outputs = self.outputs.lock().unwrap();
        if let Some(&slot) = outputs.get(output) {
            return Some(slot);
        }
        let slot = outputs.len();
        if slot == MAX_OUTPUTS {
            return None;
        }
        write_name(&self.map, output_offset(slot), output);
        self.map.u64_at(8).store(slot as u64 + 1, Ordering::Release);
        outputs.insert(output.to_owned(), slot);
        Some(slot)
    }

    /// The node just took or sent a message.
    pub fn touch(&self, now_ns: u64) {
        self.map.u64_at(16).store(now_ns, Ordering::Relaxed);
    }

    /// The node is blocked waiting for others (input, or free regions), as
    /// opposed to working: a watchdog shouldn't count that as stuck.
    pub fn set_waiting(&self, waiting: bool) {
        self.map.u64_at(24).store(waiting as u64, Ordering::Relaxed);
    }

    pub fn record_output(&self, slot: usize, bytes: u64) {
        let base = output_offset(slot);
        self.map.u64_at(base + 64).fetch_add(1, Ordering::Relaxed);
        self.map.u64_at(base + 72).fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn record_latency(&self, slot: usize, ns: u64) {
        let base = slot_offset(slot);
        self.map.u64_at(base + 128 + bucket(ns) * 8).fetch_add(1, Ordering::Relaxed);
        self.map.u64_at(base + 64).fetch_max(ns, Ordering::Relaxed);
    }

    pub fn record_processing(&self, slot: usize, ns: u64) {
        let base = slot_offset(slot);
        self.map.u64_at(base + 128 + (BUCKETS + bucket(ns)) * 8).fetch_add(1, Ordering::Relaxed);
        self.map.u64_at(base + 72).fetch_max(ns, Ordering::Relaxed);
    }
}

/// One input's histograms, as read from a stats file.
#[derive(Debug, Clone)]
pub struct InputHistograms {
    pub input: String,
    pub latency: Summary,
    pub processing: Summary,
}

/// The daemon's side of a node's stats file.
pub struct StatsReader {
    map: Mapping,
}

impl StatsReader {
    pub fn open(dir: &Path, node_id: &str) -> io::Result<Self> {
        Ok(Self { map: Mapping::open(&stats_path(dir, node_id), STATS_LEN)? })
    }

    /// When the node last took or sent a message, and whether it's waiting.
    pub fn activity(&self) -> (u64, bool) {
        (self.map.u64_at(16).load(Ordering::Relaxed), self.map.u64_at(24).load(Ordering::Relaxed) != 0)
    }

    pub fn input_name(&self, slot: usize) -> Option<String> {
        if slot >= self.map.u64_at(0).load(Ordering::Acquire) as usize {
            return None;
        }
        Some(read_name(&self.map, slot_offset(slot)))
    }

    pub fn output_name(&self, slot: usize) -> Option<String> {
        if slot >= self.map.u64_at(8).load(Ordering::Acquire) as usize {
            return None;
        }
        Some(read_name(&self.map, output_offset(slot)))
    }

    /// `(output, messages, bytes)` sent so far.
    pub fn outputs(&self) -> Vec<(String, u64, u64)> {
        let count = (self.map.u64_at(8).load(Ordering::Acquire) as usize).min(MAX_OUTPUTS);
        (0..count)
            .map(|slot| {
                let base = output_offset(slot);
                let (messages, bytes) = (self.map.u64_at(base + 64), self.map.u64_at(base + 72));
                (read_name(&self.map, base), messages.load(Ordering::Relaxed), bytes.load(Ordering::Relaxed))
            })
            .collect()
    }

    pub fn inputs(&self) -> Vec<InputHistograms> {
        let count = (self.map.u64_at(0).load(Ordering::Acquire) as usize).min(MAX_INPUTS);
        (0..count)
            .filter_map(|slot| {
                let base = slot_offset(slot);
                let buckets = |from: usize| -> Vec<u64> {
                    (0..BUCKETS).map(|i| self.map.u64_at(base + 128 + (from + i) * 8).load(Ordering::Relaxed)).collect()
                };
                Some(InputHistograms {
                    input: self.input_name(slot)?,
                    latency: Summary::of(&buckets(0), self.map.u64_at(base + 64).load(Ordering::Relaxed)),
                    processing: Summary::of(&buckets(BUCKETS), self.map.u64_at(base + 72).load(Ordering::Relaxed)),
                })
            })
            .collect()
    }
}

// The events ring: single producer (the node, under a lock since samples can
// be dropped on any thread), single consumer (the daemon).
//
//   [write: u64][read: u64][dropped: u64][padding to 64][RING_LEN × RawEvent]
//
// The producer writes an entry, then publishes it by bumping `write`
// (Release). The consumer reads entries below `write` (Acquire), then frees
// them by bumping `read` (Release).

pub const RING_LEN: usize = 4096;
const EVENT_LEN: usize = std::mem::size_of::<RawEvent>();
const EVENTS_LEN: usize = 64 + RING_LEN * EVENT_LEN;

/// What happened to a message, from the process that saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct RawEvent {
    pub kind: u64,
    pub span: u64,
    pub trace: u64,
    pub parent: u64,
    pub t_ns: u64,
    /// The input's slot in the stats file for `TAKEN` and `RELEASED`, the
    /// output's for `PUBLISHED`.
    pub aux: u64,
}

/// The sender published the message (all of `span`, `trace`, `parent` set).
pub const PUBLISHED: u64 = 1;
/// A receiver took it off its connection.
pub const TAKEN: u64 = 2;
/// That receiver dropped its sample: done processing.
pub const RELEASED: u64 = 3;

pub struct EventWriter {
    map: Mapping,
    lock: Mutex<()>,
}

impl EventWriter {
    pub fn create(dir: &Path, node_id: &str) -> io::Result<Self> {
        Ok(Self { map: Mapping::create(&events_path(dir, node_id), EVENTS_LEN)?, lock: Mutex::new(()) })
    }

    pub fn push(&self, event: RawEvent) {
        let _guard = self.lock.lock().unwrap();
        let write = self.map.u64_at(0).load(Ordering::Relaxed);
        let read = self.map.u64_at(8).load(Ordering::Acquire);
        if write - read >= RING_LEN as u64 {
            self.map.u64_at(16).fetch_add(1, Ordering::Relaxed);
            return;
        }
        let at = 64 + (write as usize % RING_LEN) * EVENT_LEN;
        // SAFETY: in bounds and aligned; the consumer is done with this entry.
        unsafe { std::ptr::write(self.map.ptr().add(at).cast::<RawEvent>(), event) };
        self.map.u64_at(0).store(write + 1, Ordering::Release);
    }
}

pub struct EventReader {
    map: Mapping,
}

impl EventReader {
    pub fn open(dir: &Path, node_id: &str) -> io::Result<Self> {
        Ok(Self { map: Mapping::open(&events_path(dir, node_id), EVENTS_LEN)? })
    }

    /// Moves every pending event into `out`.
    pub fn drain(&self, out: &mut Vec<RawEvent>) {
        let write = self.map.u64_at(0).load(Ordering::Acquire);
        let mut read = self.map.u64_at(8).load(Ordering::Relaxed);
        while read < write {
            let at = 64 + (read as usize % RING_LEN) * EVENT_LEN;
            // SAFETY: in bounds and aligned; published by the producer.
            out.push(unsafe { std::ptr::read(self.map.ptr().add(at).cast::<RawEvent>()) });
            read += 1;
        }
        self.map.u64_at(8).store(read, Ordering::Release);
    }

    /// Events the node couldn't record because the ring was full.
    pub fn dropped(&self) -> u64 {
        self.map.u64_at(16).load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_are_ordered_and_tight() {
        let mut last = 0;
        for ns in (0..100_000).chain([1 << 20, 123_456_789, 1 << 34]) {
            let b = bucket(ns);
            assert!(b >= last, "not monotonic at {ns}");
            last = b;
            let v = bucket_value(b);
            assert!(v.abs_diff(ns) <= ns / 8 + 1, "{ns} -> bucket {b} -> {v}");
        }
        assert!(bucket(u64::MAX) < BUCKETS);
    }

    #[test]
    fn summaries() {
        let mut buckets = vec![0; BUCKETS];
        for ns in 1..=1000u64 {
            buckets[bucket(ns * 1000)] += 1;
        }
        let s = Summary::of(&buckets, 1_000_000);
        assert_eq!(s.count, 1000);
        assert!(s.p50.abs_diff(500_000) < 500_000 / 8, "{s:?}");
        assert!(s.p99.abs_diff(990_000) < 990_000 / 8, "{s:?}");
        assert_eq!(s.max, 1_000_000);
    }

    #[test]
    fn stats_and_events_round_trip() {
        let dir = std::env::temp_dir().join(format!("keel-trace-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let stats = StatsWriter::create(&dir, "n").unwrap();
        let slot = stats.slot("frames").unwrap();
        assert_eq!(stats.slot("frames"), Some(slot));
        stats.record_latency(slot, 20_000);
        stats.record_processing(slot, 3_000);
        let read = StatsReader::open(&dir, "n").unwrap().inputs();
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].input, "frames");
        assert_eq!((read[0].latency.count, read[0].latency.max), (1, 20_000));
        assert_eq!(read[0].processing.max, 3_000);
        let out = stats.output_slot("brightness").unwrap();
        stats.record_output(out, 16);
        stats.record_output(out, 16);
        assert_eq!(StatsReader::open(&dir, "n").unwrap().outputs(), [("brightness".into(), 2, 32)]);

        let writer = EventWriter::create(&dir, "n").unwrap();
        let reader = EventReader::open(&dir, "n").unwrap();
        let event = |span| RawEvent { kind: TAKEN, span, trace: 0, parent: 0, t_ns: span, aux: 0 };
        for span in 0..RING_LEN as u64 + 5 {
            writer.push(event(span));
        }
        let mut out = Vec::new();
        reader.drain(&mut out);
        assert_eq!(out.len(), RING_LEN);
        assert_eq!(out[7], event(7));
        assert_eq!(reader.dropped(), 5);
        writer.push(event(99));
        out.clear();
        reader.drain(&mut out);
        assert_eq!(out, [event(99)]);

        std::fs::remove_dir_all(dir).unwrap();
    }
}
