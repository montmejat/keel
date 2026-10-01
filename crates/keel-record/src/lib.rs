//! Recordings: what a recorder node saw, to replay it or make datasets of it.
//!
//! A recording is one file:
//!
//! ```text
//! KEELREC1\n
//! <header: one JSON line>\n
//! records: [len: u32][channel: u16][span: u64][t_ns: u64][payload: len - 18 bytes]
//! ```
//!
//! All integers little-endian. A channel is one recorded input, numbered in
//! the order the header lists them. `span` is the message's trace span, so a
//! record can be matched with `keel trace`. `t_ns` is when it was published,
//! on the recording machine's monotonic clock (converted there if it came
//! from another machine): replays keep the original spacing.
//!
//! The header says which dataflow and deployment produced the data. There is
//! no index: records are read in order, which is what replay and export do.
//!
//! A recorder can also keep only the last seconds, in memory, for the daemon
//! to save when a node fails: see [`flight`].

pub mod flight;

use std::fs::File;
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};

pub const MAGIC: &[u8] = b"KEELREC1\n";
const RECORD_HEADER: usize = 2 + 8 + 8;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Header {
    pub dataflow: String,
    /// The deployment the recorded nodes ran from, if any.
    pub deployment: Option<String>,
    /// The recorder node's id.
    pub recorder: String,
    /// Unix seconds.
    pub started: u64,
    pub channels: Vec<Channel>,
    /// For what a flight recorder held when a node failed: which, and how.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<Failure>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failure {
    pub node: String,
    /// How it ended, e.g. `exit status: 1`.
    pub status: String,
    /// Unix seconds.
    pub at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Channel {
    /// The recorder's input.
    pub input: String,
    /// `node/output` feeding it.
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub channel: u16,
    pub span: u64,
    pub t_ns: u64,
    pub payload: Vec<u8>,
}

pub struct Writer<W: Write> {
    w: W,
}

impl Writer<BufWriter<File>> {
    pub fn create(path: &Path, header: &Header) -> io::Result<Self> {
        Self::new(BufWriter::with_capacity(1 << 20, File::create(path)?), header)
    }
}

impl<W: Write> Writer<W> {
    pub fn new(mut w: W, header: &Header) -> io::Result<Self> {
        w.write_all(MAGIC)?;
        serde_json::to_writer(&mut w, header)?;
        w.write_all(b"\n")?;
        Ok(Self { w })
    }

    pub fn write(&mut self, channel: u16, span: u64, t_ns: u64, payload: &[u8]) -> io::Result<()> {
        let len = u32::try_from(RECORD_HEADER + payload.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "message too large to record"))?;
        self.w.write_all(&len.to_le_bytes())?;
        self.w.write_all(&channel.to_le_bytes())?;
        self.w.write_all(&span.to_le_bytes())?;
        self.w.write_all(&t_ns.to_le_bytes())?;
        self.w.write_all(payload)
    }

    pub fn flush(&mut self) -> io::Result<()> {
        self.w.flush()
    }
}

pub struct Reader<R: Read> {
    r: R,
    pub header: Header,
    /// The last record was cut short: the recorder was killed mid-write.
    pub truncated: bool,
}

impl Reader<BufReader<File>> {
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path).map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", path.display())))?;
        Self::new(BufReader::with_capacity(1 << 20, file))
    }
}

impl<R: BufRead> Reader<R> {
    pub fn new(mut r: R) -> io::Result<Self> {
        let mut magic = [0u8; MAGIC.len()];
        r.read_exact(&mut magic)?;
        if magic != MAGIC {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "not a keel recording"));
        }
        let mut line = String::new();
        r.read_line(&mut line)?;
        let header = serde_json::from_str(&line)?;
        Ok(Self { r, header, truncated: false })
    }

    /// The next record; `None` at the end, including a cut-short last one.
    pub fn next_record(&mut self) -> io::Result<Option<Record>> {
        let mut len = [0u8; 4];
        match self.r.read_exact(&mut len) {
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            res => res?,
        }
        let len = u32::from_le_bytes(len) as usize;
        if len < RECORD_HEADER {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "corrupt record"));
        }
        let mut body = vec![0u8; len];
        if let Err(e) = self.r.read_exact(&mut body) {
            if e.kind() == io::ErrorKind::UnexpectedEof {
                self.truncated = true;
                return Ok(None);
            }
            return Err(e);
        }
        let u64_at = |at: usize| u64::from_le_bytes(body[at..at + 8].try_into().unwrap());
        Ok(Some(Record {
            channel: u16::from_le_bytes([body[0], body[1]]),
            span: u64_at(2),
            t_ns: u64_at(10),
            payload: body.split_off(RECORD_HEADER),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_truncation() {
        let header = Header {
            dataflow: "pipeline".into(),
            deployment: Some("5b2fabe42087".into()),
            recorder: "rec".into(),
            started: 1,
            channels: vec![Channel { input: "frames".into(), source: "camera/frames".into() }],
            failure: None,
        };
        let mut file = Vec::new();
        let mut w = Writer::new(&mut file, &header).unwrap();
        w.write(0, 7, 100, b"first").unwrap();
        w.write(0, 8, 200, &[9; 1000]).unwrap();
        w.flush().unwrap();

        let mut r = Reader::new(file.as_slice()).unwrap();
        assert_eq!(r.header.channels, header.channels);
        assert_eq!(
            r.next_record().unwrap().unwrap(),
            Record { channel: 0, span: 7, t_ns: 100, payload: b"first".to_vec() }
        );
        assert_eq!(r.next_record().unwrap().unwrap().payload.len(), 1000);
        assert_eq!(r.next_record().unwrap(), None);
        assert!(!r.truncated);

        let cut = &file[..file.len() - 10];
        let mut r = Reader::new(cut).unwrap();
        r.next_record().unwrap().unwrap();
        assert_eq!(r.next_record().unwrap(), None);
        assert!(r.truncated);
    }
}
