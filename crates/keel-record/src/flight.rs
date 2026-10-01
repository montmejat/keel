//! The flight recorder: the last seconds of a recorder's inputs, kept in
//! memory and saved only when something goes wrong.
//!
//! The recorder writes short recordings (segments) one after the other into
//! the dataflow's shared-memory directory, and deletes those older than the
//! time it's asked to keep. Shared memory is RAM, so nothing touches the
//! disk, and files there outlive any process: when a node fails the daemon
//! joins the segments into one ordinary recording, whatever state the
//! recorder itself is in.

use std::fs::{self, File};
use std::io::{self, BufWriter};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::{Failure, Header, Reader, Writer};

/// The longest a segment covers: what's kept overshoots by at most this.
const MAX_SEGMENT: Duration = Duration::from_secs(1);

/// Where flight recorders keep their segments, one directory each.
pub fn root(shm_dir: &Path) -> PathBuf {
    shm_dir.join("flight")
}

fn segment(dir: &Path, n: u64) -> PathBuf {
    dir.join(format!("{n:010}.keel"))
}

/// The segments of one recorder, being written.
pub struct Ring {
    dir: PathBuf,
    header: Header,
    writer: Writer<BufWriter<File>>,
    /// The segment being written, and since when.
    current: (u64, Instant),
    segment_len: Duration,
    /// Segments kept besides the current one.
    kept: u64,
}

impl Ring {
    /// Starts over in `dir`, keeping at least the last `last` of what's
    /// written.
    pub fn create(dir: PathBuf, header: Header, last: Duration) -> io::Result<Self> {
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir)?;
        let segment_len = (last / 4).min(MAX_SEGMENT).max(Duration::from_millis(1));
        let kept = last.as_nanos().div_ceil(segment_len.as_nanos()) as u64;
        let writer = Writer::create(&segment(&dir, 0), &header)?;
        Ok(Self { dir, header, writer, current: (0, Instant::now()), segment_len, kept })
    }

    /// Writes one record where the daemon can read it at once.
    pub fn write(&mut self, channel: u16, span: u64, t_ns: u64, payload: &[u8]) -> io::Result<()> {
        let (n, since) = self.current;
        if since.elapsed() >= self.segment_len {
            self.writer = Writer::create(&segment(&self.dir, n + 1), &self.header)?;
            self.current = (n + 1, Instant::now());
            if let Some(old) = n.checked_sub(self.kept) {
                let _ = fs::remove_file(segment(&self.dir, old));
            }
        }
        self.writer.write(channel, span, t_ns, payload)?;
        self.writer.flush()
    }
}

/// Joins the segments in `dir` into one recording, `to`, marked with the
/// failure that made it worth keeping. Returns how many messages it holds
/// and the time they span, or `None` (and no file) if there were none.
pub fn save(dir: &Path, to: &Path, failure: Failure) -> io::Result<Option<(u64, Duration)>> {
    let mut segments: Vec<PathBuf> = fs::read_dir(dir)?.flatten().map(|e| e.path()).collect();
    segments.sort();
    let mut writer = None;
    let (mut count, mut first, mut last) = (0, u64::MAX, 0);
    for path in segments {
        // The recorder is still running: a segment can vanish, or be too new
        // to have its header.
        let Ok(mut reader) = Reader::open(&path) else { continue };
        if writer.is_none() {
            let header = Header { failure: Some(failure.clone()), ..reader.header.clone() };
            writer = Some(Writer::create(to, &header)?);
        }
        while let Some(record) = reader.next_record()? {
            writer.as_mut().unwrap().write(record.channel, record.span, record.t_ns, &record.payload)?;
            (count, first, last) = (count + 1, first.min(record.t_ns), last.max(record.t_ns));
        }
    }
    match writer {
        Some(mut writer) if count > 0 => {
            writer.flush()?;
            Ok(Some((count, Duration::from_nanos(last - first))))
        }
        Some(_) => fs::remove_file(to).map(|()| None),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Channel;

    #[test]
    fn keeps_the_last_and_saves_it() {
        let dir = std::env::temp_dir().join(format!("keel-flight-test-{}", std::process::id()));
        let header = Header {
            dataflow: "robot".into(),
            deployment: None,
            recorder: "blackbox".into(),
            started: 1,
            channels: vec![Channel { input: "count".into(), source: "counter/count".into() }],
            failure: None,
        };
        let failure = Failure { node: "flaky".into(), status: "exit status: 1".into(), at: 2 };
        let (ring_dir, saved) = (dir.join("ring"), dir.join("saved.keel"));

        // 20 ms kept, in 5 ms segments: 100 ms of messages leaves the last few.
        let mut ring = Ring::create(ring_dir.clone(), header, Duration::from_millis(20)).unwrap();
        assert_eq!(save(&ring_dir, &saved, failure.clone()).unwrap(), None, "nothing recorded yet");
        assert!(!saved.exists());
        for n in 0..100u64 {
            ring.write(0, n, n * 1_000_000, &n.to_le_bytes()).unwrap();
            std::thread::sleep(Duration::from_millis(1));
        }
        let (count, span) = save(&ring_dir, &saved, failure.clone()).unwrap().unwrap();
        assert!((15..40).contains(&count), "{count} messages kept");
        assert_eq!(span, Duration::from_millis(count - 1));

        let mut reader = Reader::open(&saved).unwrap();
        assert_eq!(reader.header.failure, Some(failure));
        let mut spans = Vec::new();
        while let Some(record) = reader.next_record().unwrap() {
            spans.push(record.span);
        }
        assert_eq!(spans, (100 - count..100).collect::<Vec<_>>(), "the newest, in order");
        fs::remove_dir_all(dir).unwrap();
    }
}
