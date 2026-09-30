//! Shared-memory regions carrying message payloads between processes on one
//! machine.
//!
//! Each node owns a small pool of regions, one file per region in the
//! daemon's shared-memory directory, named `<node>.<slot>`. A region starts
//! with a header holding a reference count and the message's trace context
//! (see [`crate::trace`]), followed by the payload:
//!
//! ```text
//! [refcount: u32][flags: u32][span: u64][trace: u64][parent: u64][published: u64][padding to 64][payload ...]
//! ```
//!
//! A region is free when its count is 0. The sender sets it to 1 (a reference
//! held by the message in transit), the daemon adds one per receiver it
//! delivers to and then drops the transit reference, and each receiver drops
//! its own when it is done with the sample. Only a free region is ever written
//! to or grown, so readers never see a payload change under them.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::trace::Context;

const HEADER_LEN: usize = 64;
const MIN_CAPACITY: usize = 4096 - HEADER_LEN;
/// Regions per node. Bounds how many messages a node can have in flight.
pub const MAX_SLOTS: usize = 32;
/// How long `Pool::acquire` waits for receivers to release a region.
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(10);

pub fn region_path(dir: &Path, node_id: &str, slot: u32) -> PathBuf {
    dir.join(format!("{node_id}.{slot}"))
}

/// One mapping of a region file.
pub struct Region {
    ptr: NonNull<u8>,
    map_len: usize,
}

// SAFETY: the mapping is plain shared memory; access to the payload is
// coordinated through the reference count.
unsafe impl Send for Region {}
unsafe impl Sync for Region {}

impl Region {
    /// Maps an existing region file in full.
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        Self::map(&file)
    }

    fn map(file: &File) -> io::Result<Self> {
        let map_len = file.metadata()?.len() as usize;
        if map_len < HEADER_LEN {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "region file is too short"));
        }
        // SAFETY: mapping a file we hold open; the result is checked below.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                map_len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                std::os::fd::AsRawFd::as_raw_fd(file),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { ptr: NonNull::new(ptr.cast()).unwrap(), map_len })
    }

    pub fn refcount(&self) -> &AtomicU32 {
        // SAFETY: the mapping is page-aligned and at least HEADER_LEN long.
        unsafe { &*self.ptr.as_ptr().cast::<AtomicU32>() }
    }

    fn header_u64(&self, index: usize) -> &AtomicU64 {
        // SAFETY: indexes 1..=4 are 8-byte aligned words within HEADER_LEN.
        unsafe { &*self.ptr.as_ptr().cast::<AtomicU64>().add(index) }
    }

    /// The trace context of the message in this region. Readers must hold a
    /// reference; the writer sets it before `publish`.
    pub fn context(&self) -> Context {
        // SAFETY: word 0 holds the refcount then the flags.
        let flags = unsafe { &*self.ptr.as_ptr().cast::<AtomicU32>().add(1) };
        Context {
            span: self.header_u64(1).load(Ordering::Relaxed),
            trace: self.header_u64(2).load(Ordering::Relaxed),
            parent: self.header_u64(3).load(Ordering::Relaxed),
            published_ns: self.header_u64(4).load(Ordering::Relaxed),
            sampled: flags.load(Ordering::Relaxed) & 1 != 0,
        }
    }

    /// Only while no one else holds a reference: the sender before `publish`,
    /// or the daemon filling a region it received from another machine.
    pub fn set_context(&self, context: &Context) {
        // SAFETY: as in `context`.
        let flags = unsafe { &*self.ptr.as_ptr().cast::<AtomicU32>().add(1) };
        flags.store(context.sampled as u32, Ordering::Relaxed);
        self.header_u64(1).store(context.span, Ordering::Relaxed);
        self.header_u64(2).store(context.trace, Ordering::Relaxed);
        self.header_u64(3).store(context.parent, Ordering::Relaxed);
        self.header_u64(4).store(context.published_ns, Ordering::Relaxed);
    }

    /// Payload bytes this mapping covers.
    pub fn capacity(&self) -> usize {
        self.map_len - HEADER_LEN
    }

    fn payload(&self) -> *mut u8 {
        // SAFETY: HEADER_LEN is within the mapping.
        unsafe { self.ptr.as_ptr().add(HEADER_LEN) }
    }

    /// # Safety
    /// The caller must hold a reference, and `len` must be within capacity.
    pub unsafe fn payload_slice(&self, len: usize) -> &[u8] {
        std::slice::from_raw_parts(self.payload(), len)
    }
}

impl Drop for Region {
    fn drop(&mut self) {
        // SAFETY: unmapping exactly what `map` mapped.
        unsafe { libc::munmap(self.ptr.as_ptr().cast(), self.map_len) };
    }
}

/// A whole file mapped shared, for the tracing files next to the regions.
pub struct Mapping {
    ptr: NonNull<u8>,
    len: usize,
}

// SAFETY: plain shared memory, accessed through atomics or under the
// protocols documented by its users.
unsafe impl Send for Mapping {}
unsafe impl Sync for Mapping {}

impl Mapping {
    /// Creates `path` with `len` zeroed bytes (sparse: pages cost memory only
    /// once touched) and maps it.
    pub fn create(path: &Path, len: usize) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).create(true).truncate(true).open(path)?;
        file.set_len(len as u64)?;
        Self::map(&file, len)
    }

    /// Maps an existing file, which must be at least `len` bytes.
    pub fn open(path: &Path, len: usize) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        if (file.metadata()?.len() as usize) < len {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("{} is too short", path.display())));
        }
        Self::map(&file, len)
    }

    fn map(file: &File, len: usize) -> io::Result<Self> {
        // SAFETY: mapping a file we hold open, checked below.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                std::os::fd::AsRawFd::as_raw_fd(file),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { ptr: NonNull::new(ptr.cast()).unwrap(), len })
    }

    pub fn ptr(&self) -> *mut u8 {
        self.ptr.as_ptr()
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The `AtomicU32` at byte `offset`, which must be 4-byte aligned.
    pub fn u32_at(&self, offset: usize) -> &AtomicU32 {
        assert!(offset.is_multiple_of(4) && offset + 4 <= self.len);
        // SAFETY: aligned and in bounds, checked above.
        unsafe { &*self.ptr.as_ptr().add(offset).cast::<AtomicU32>() }
    }

    /// The `AtomicU64` at byte `offset`, which must be 8-byte aligned.
    pub fn u64_at(&self, offset: usize) -> &AtomicU64 {
        assert!(offset.is_multiple_of(8) && offset + 8 <= self.len);
        // SAFETY: aligned and in bounds, checked above.
        unsafe { &*self.ptr.as_ptr().add(offset).cast::<AtomicU64>() }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: unmapping exactly what `map` mapped.
        unsafe { libc::munmap(self.ptr.as_ptr().cast(), self.len) };
    }
}

/// The regions a node sends from.
pub struct Pool {
    dir: PathBuf,
    node_id: String,
    slots: Vec<(File, Region)>,
}

impl Pool {
    pub fn new(dir: PathBuf, node_id: String) -> Self {
        Self { dir, node_id, slots: Vec::new() }
    }

    /// Finds a free region with room for `len` bytes, growing or creating
    /// one if needed. Waits while every slot is in use.
    pub fn acquire(&mut self, len: usize) -> io::Result<u32> {
        let deadline = Instant::now() + ACQUIRE_TIMEOUT;
        let mut spins = 0u32;
        loop {
            let free = |r: &Region| r.refcount().load(Ordering::Acquire) == 0;
            if let Some(i) = self.slots.iter().position(|(_, r)| free(r) && r.capacity() >= len) {
                return Ok(i as u32);
            }
            if self.slots.len() < MAX_SLOTS {
                let slot = self.slots.len() as u32;
                let file = OpenOptions::new().read(true).write(true).create_new(true).open(region_path(
                    &self.dir,
                    &self.node_id,
                    slot,
                ))?;
                let region = Self::resize(&file, len)?;
                self.slots.push((file, region));
                return Ok(slot);
            }
            // All slots exist: grow the biggest free one.
            let biggest_free =
                (0..self.slots.len()).filter(|&i| free(&self.slots[i].1)).max_by_key(|&i| self.slots[i].1.capacity());
            if let Some(i) = biggest_free {
                self.slots[i].1 = Self::resize(&self.slots[i].0, len)?;
                return Ok(i as u32);
            }
            if Instant::now() > deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("all {MAX_SLOTS} shared-memory regions are still held by receivers"),
                ));
            }
            spins += 1;
            if spins < 100 {
                std::thread::yield_now();
            } else {
                std::thread::sleep(Duration::from_micros(50));
            }
        }
    }

    /// Payload of a slot returned by `acquire`, to be filled before `publish`.
    pub fn payload_mut(&mut self, slot: u32, len: usize) -> &mut [u8] {
        let region = &self.slots[slot as usize].1;
        assert!(len <= region.capacity());
        // SAFETY: the slot is free (refcount 0), so no one else reads it, and
        // `&mut self` keeps us from handing out two slices.
        unsafe { std::slice::from_raw_parts_mut(region.payload(), len) }
    }

    pub fn region(&self, slot: u32) -> &Region {
        &self.slots[slot as usize].1
    }

    /// Takes the in-transit reference, right before the message is sent.
    pub fn publish(&self, slot: u32) {
        self.slots[slot as usize].1.refcount().store(1, Ordering::Release);
    }

    fn resize(file: &File, len: usize) -> io::Result<Region> {
        let capacity = len.max(MIN_CAPACITY);
        file.set_len((HEADER_LEN + capacity).next_power_of_two() as u64)?;
        Region::map(file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_are_reused_only_once_released() {
        let dir = std::env::temp_dir().join(format!("keel-shm-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut pool = Pool::new(dir.clone(), "n".into());

        let a = pool.acquire(10).unwrap();
        pool.payload_mut(a, 3).copy_from_slice(b"abc");
        pool.publish(a);
        let b = pool.acquire(10).unwrap();
        assert_ne!(a, b, "a published slot must not be handed out again");

        // Another process maps the same region and releases it.
        let reader = Region::open(&region_path(&dir, "n", a)).unwrap();
        assert_eq!(unsafe { reader.payload_slice(3) }, b"abc");
        reader.refcount().fetch_sub(1, Ordering::Release);
        assert_eq!(pool.acquire(10).unwrap(), a);

        // A free slot grows to fit a bigger payload.
        let big = pool.acquire(1 << 20).unwrap();
        assert!(pool.slots[big as usize].1.capacity() >= 1 << 20);

        std::fs::remove_dir_all(dir).unwrap();
    }
}
