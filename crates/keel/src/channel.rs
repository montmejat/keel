//! How messages go from node to node without the daemon: channels and bells,
//! both small files next to the regions in the session's shared-memory
//! directory, created by the daemon before the nodes start.
//!
//! A *channel* carries descriptors (`slot`, `len`) of one input, from its one
//! sender to its one receiver:
//!
//! ```text
//! [write: u64][read: u64][keep: u64][mailbox: u64][padding to 64][CAPACITY × descriptor]
//! ```
//!
//! With `keep: all` it's a ring: the sender writes an entry then bumps
//! `write` (Release), the receiver reads entries below `write` (Acquire) then
//! bumps `read`. A sender can't have more than `MAX_SLOTS` messages in flight,
//! so the ring never fills. With `keep: latest` only `mailbox` is used: the
//! sender swaps its descriptor in and releases the one it displaced.
//!
//! A *bell* wakes a receiver up, whichever input a message arrived on:
//!
//! ```text
//! [seq: u32][waiting: u32][stop: u32]
//! ```
//!
//! Senders bump `seq` after pushing, and call `FUTEX_WAKE` only if the
//! receiver said it's `waiting`. The receiver reads `seq`, checks its
//! channels, and sleeps with `FUTEX_WAIT` only if `seq` hasn't moved: a
//! message pushed in between can't be missed. `stop` is how the daemon says
//! all upstream nodes are gone.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use crate::shm::{Mapping, Region};

/// Entries in a `keep: all` channel. At least `shm::MAX_SLOTS`, so a sender
/// never finds it full.
pub const CAPACITY: usize = 64;
const CHANNEL_LEN: usize = 64 + CAPACITY * 8;
const BELL_LEN: usize = 64;

/// Owner name of the daemon's bell and channels. Node ids can't contain `@`.
pub const DAEMON: &str = "@daemon";

/// What an input keeps when its receiver falls behind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Keep {
    /// Every message, in order; the sender waits when it runs out of regions.
    #[default]
    All,
    /// Only the newest message; older ones are dropped, the sender never waits.
    Latest,
}

pub fn channel_path(dir: &Path, node: &str, input: &str) -> PathBuf {
    dir.join(format!("{node}.in.{input}"))
}

/// The channel the daemon reads to forward `source/output` to other machines.
pub fn forward_path(dir: &Path, source: &str, output: &str) -> PathBuf {
    dir.join(format!("{DAEMON}.fwd.{source}.{output}"))
}

pub fn bell_path(dir: &Path, owner: &str) -> PathBuf {
    dir.join(format!("{owner}.bell"))
}

fn pack(slot: u32, len: u64) -> u64 {
    debug_assert!(slot < 255 && len < 1 << 56);
    (len << 8) | (slot as u64 + 1)
}

fn unpack(word: u64) -> Option<(u32, u64)> {
    (word != 0).then(|| ((word & 0xff) as u32 - 1, word >> 8))
}

pub struct Channel {
    map: Mapping,
    keep: Keep,
}

impl Channel {
    pub fn create(path: &Path, keep: Keep) -> io::Result<Self> {
        let map = Mapping::create(path, CHANNEL_LEN)?;
        map.u64_at(16).store(keep as u64, Ordering::Relaxed);
        Ok(Self { map, keep })
    }

    pub fn open(path: &Path) -> io::Result<Self> {
        let map = Mapping::open(path, CHANNEL_LEN)?;
        let keep = if map.u64_at(16).load(Ordering::Relaxed) == Keep::Latest as u64 { Keep::Latest } else { Keep::All };
        Ok(Self { map, keep })
    }

    pub fn keep(&self) -> Keep {
        self.keep
    }

    /// Sender side. Returns the descriptor a `keep: latest` channel dropped,
    /// whose reference the sender must release.
    pub fn push(&self, slot: u32, len: u64) -> Option<(u32, u64)> {
        let word = pack(slot, len);
        if self.keep == Keep::Latest {
            return unpack(self.map.u64_at(24).swap(word, Ordering::AcqRel));
        }
        let (write, read) = (self.map.u64_at(0), self.map.u64_at(8));
        let w = write.load(Ordering::Relaxed);
        // Can't happen while CAPACITY >= MAX_SLOTS, but never overwrite.
        while w - read.load(Ordering::Acquire) >= CAPACITY as u64 {
            std::thread::yield_now();
        }
        self.map.u64_at(64 + (w as usize % CAPACITY) * 8).store(word, Ordering::Relaxed);
        write.store(w + 1, Ordering::Release);
        None
    }

    /// Receiver side.
    pub fn pop(&self) -> Option<(u32, u64)> {
        if self.keep == Keep::Latest {
            return unpack(self.map.u64_at(24).swap(0, Ordering::AcqRel));
        }
        let (write, read) = (self.map.u64_at(0), self.map.u64_at(8));
        let r = read.load(Ordering::Relaxed);
        if r == write.load(Ordering::Acquire) {
            return None;
        }
        let word = self.map.u64_at(64 + (r as usize % CAPACITY) * 8).load(Ordering::Relaxed);
        read.store(r + 1, Ordering::Release);
        unpack(word)
    }
}

pub struct Bell {
    map: Mapping,
}

impl Bell {
    pub fn create(path: &Path) -> io::Result<Self> {
        Ok(Self { map: Mapping::create(path, BELL_LEN)? })
    }

    pub fn open(path: &Path) -> io::Result<Self> {
        Ok(Self { map: Mapping::open(path, BELL_LEN)? })
    }

    /// Wakes the receiver if it's asleep. Call after pushing.
    pub fn ring(&self) {
        self.map.u32_at(0).fetch_add(1, Ordering::SeqCst);
        if self.map.u32_at(4).load(Ordering::SeqCst) != 0 {
            futex(self.map.u32_at(0).as_ptr(), libc::FUTEX_WAKE, i32::MAX as u32, None);
        }
    }

    /// Read before checking the channels, then passed to [`Bell::wait`].
    pub fn seq(&self) -> u32 {
        self.map.u32_at(0).load(Ordering::SeqCst)
    }

    /// Sleeps until the bell rings after `seen`, or `timeout` passes.
    pub fn wait(&self, seen: u32, timeout: Option<Duration>) {
        self.map.u32_at(4).store(1, Ordering::SeqCst);
        if self.map.u32_at(0).load(Ordering::SeqCst) == seen {
            futex(self.map.u32_at(0).as_ptr(), libc::FUTEX_WAIT, seen, timeout);
        }
        self.map.u32_at(4).store(0, Ordering::SeqCst);
    }

    pub fn request_stop(&self) {
        self.map.u32_at(8).store(1, Ordering::SeqCst);
        self.ring();
    }

    pub fn stop_requested(&self) -> bool {
        self.map.u32_at(8).load(Ordering::SeqCst) != 0
    }
}

/// Shared between processes, so not `FUTEX_PRIVATE_FLAG`.
fn futex(word: *mut u32, op: i32, value: u32, timeout: Option<Duration>) {
    let ts = timeout.map(|t| libc::timespec { tv_sec: t.as_secs() as _, tv_nsec: t.subsec_nanos() as _ });
    let ts_ptr = ts.as_ref().map_or(std::ptr::null(), |t| t as *const libc::timespec);
    // SAFETY: `word` points into a live shared mapping. EINTR, EAGAIN and
    // ETIMEDOUT all just mean "go check again".
    unsafe { libc::syscall(libc::SYS_futex, word, op, value, ts_ptr, std::ptr::null::<u32>(), 0) };
}

/// One receiver of an output: its channel, and the bell of its owner.
pub struct Target {
    pub channel: Channel,
    pub bell: Arc<Bell>,
}

/// Hands a message in `slot` of the sender's pool to every target: one
/// reference each, taken before any of them can see it. `region(slot)` looks
/// up the sender's regions, to release those a `keep: latest` target dropped.
pub fn send<'a>(targets: &[Target], slot: u32, len: u64, region: impl Fn(u32) -> &'a Region) {
    region(slot).refcount().store(targets.len() as u32, Ordering::Release);
    for target in targets {
        if let Some((dropped, _)) = target.channel.push(slot, len) {
            region(dropped).refcount().fetch_sub(1, Ordering::Release);
        }
        target.bell.ring();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("keel-channel-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn queues_keep_everything_in_order() {
        let dir = dir();
        let sender = Channel::create(&dir.join("all"), Keep::All).unwrap();
        let receiver = Channel::open(&dir.join("all")).unwrap();
        assert_eq!(receiver.pop(), None);
        for i in 0..CAPACITY as u32 * 3 {
            assert_eq!(sender.push(i % 32, i as u64 * 1000), None);
            assert_eq!(receiver.pop(), Some((i % 32, i as u64 * 1000)));
        }
        sender.push(1, 1);
        sender.push(2, 2);
        assert_eq!((receiver.pop(), receiver.pop(), receiver.pop()), (Some((1, 1)), Some((2, 2)), None));
    }

    #[test]
    fn latest_keeps_one() {
        let dir = dir();
        let sender = Channel::create(&dir.join("latest"), Keep::Latest).unwrap();
        let receiver = Channel::open(&dir.join("latest")).unwrap();
        assert_eq!(receiver.keep(), Keep::Latest);
        assert_eq!(sender.push(0, 10), None);
        assert_eq!(sender.push(1, 20), Some((0, 10)));
        assert_eq!(receiver.pop(), Some((1, 20)));
        assert_eq!(receiver.pop(), None);
    }

    #[test]
    fn bells_wake_sleepers() {
        let dir = dir();
        let bell = Arc::new(Bell::create(&dir.join("b.bell")).unwrap());
        let seen = bell.seq();
        let sleeper = {
            let bell = Bell::open(&dir.join("b.bell")).unwrap();
            std::thread::spawn(move || {
                let start = std::time::Instant::now();
                bell.wait(seen, Some(Duration::from_secs(5)));
                start.elapsed()
            })
        };
        std::thread::sleep(Duration::from_millis(50));
        bell.ring();
        assert!(sleeper.join().unwrap() < Duration::from_secs(1));
        // A ring before the wait isn't missed.
        let seen = bell.seq();
        bell.ring();
        bell.wait(seen, Some(Duration::from_secs(5)));
        assert!(!bell.stop_requested());
        bell.request_stop();
        assert!(bell.stop_requested());
    }
}
