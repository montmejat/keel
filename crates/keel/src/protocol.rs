//! Wire protocols: daemon <-> nodes over a Unix socket, and daemon <-> daemon
//! over TCP.
//!
//! Every frame is `[u32 LE length][u8 tag][fields]`; each field is
//! `[u32 LE length][bytes]`. Hand-rolled and dependency-free on purpose.

use std::io::{self, Read, Write};

/// Set by the daemon on every node it spawns.
pub const ENV_NODE_ID: &str = "KEEL_NODE_ID";
pub const ENV_DAEMON_SOCKET: &str = "KEEL_DAEMON_SOCKET";
/// Directory holding the shared-memory regions, see [`crate::shm`].
pub const ENV_SHM_DIR: &str = "KEEL_SHM_DIR";

const MAX_FRAME_LEN: u32 = 64 * 1024 * 1024;

/// Node -> daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeMsg {
    Register {
        node_id: String,
    },
    /// The payload is in region `<this node>.<slot>`.
    Output {
        output_id: String,
        slot: u32,
        len: u64,
    },
}

/// Daemon -> node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonMsg {
    /// All nodes have registered, so no output can be lost.
    Ready,
    /// The payload is in region `<source>.<slot>`, and the receiver holds a
    /// reference to it.
    Input { input_id: String, source: String, slot: u32, len: u64 },
    /// All upstream nodes have exited.
    Stop,
}

impl NodeMsg {
    pub fn write_to(&self, w: &mut impl Write) -> io::Result<()> {
        match self {
            NodeMsg::Register { node_id } => write_frame(w, 1, &[node_id.as_bytes()]),
            NodeMsg::Output { output_id, slot, len } => {
                write_frame(w, 2, &[output_id.as_bytes(), &slot.to_le_bytes(), &len.to_le_bytes()])
            }
        }
    }

    /// `Ok(None)` on a clean end of stream.
    pub fn read_from(r: &mut impl Read) -> io::Result<Option<Self>> {
        let Some((tag, mut f)) = read_frame(r)? else {
            return Ok(None);
        };
        let msg = match tag {
            1 => NodeMsg::Register { node_id: f.string()? },
            2 => NodeMsg::Output { output_id: f.string()?, slot: f.u32()?, len: f.u64()? },
            t => return Err(invalid(format!("unknown node message tag {t}"))),
        };
        Ok(Some(msg))
    }
}

impl DaemonMsg {
    pub fn write_to(&self, w: &mut impl Write) -> io::Result<()> {
        match self {
            DaemonMsg::Ready => write_frame(w, 101, &[]),
            DaemonMsg::Input { input_id, source, slot, len } => {
                write_frame(w, 102, &[input_id.as_bytes(), source.as_bytes(), &slot.to_le_bytes(), &len.to_le_bytes()])
            }
            DaemonMsg::Stop => write_frame(w, 103, &[]),
        }
    }

    /// `Ok(None)` on a clean end of stream.
    pub fn read_from(r: &mut impl Read) -> io::Result<Option<Self>> {
        let Some((tag, mut f)) = read_frame(r)? else {
            return Ok(None);
        };
        let msg = match tag {
            101 => DaemonMsg::Ready,
            102 => DaemonMsg::Input { input_id: f.string()?, source: f.string()?, slot: f.u32()?, len: f.u64()? },
            103 => DaemonMsg::Stop,
            t => return Err(invalid(format!("unknown daemon message tag {t}"))),
        };
        Ok(Some(msg))
    }
}

/// Daemon -> daemon, over TCP: what local nodes send to nodes on the other
/// machine. One connection per direction and machine pair, so everything a
/// node sends arrives in order, `Closed` last.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerMsg {
    /// `source/output` sent a message; its payload travels with it.
    Data { source: String, output: String, payload: Vec<u8> },
    /// `node` has exited: nothing more will come from it.
    Closed { node: String },
}

impl PeerMsg {
    pub fn write_to(&self, w: &mut impl Write) -> io::Result<()> {
        match self {
            PeerMsg::Data { source, output, payload } => Self::write_data(w, source, output, payload),
            PeerMsg::Closed { node } => write_frame(w, 202, &[node.as_bytes()]),
        }
    }

    /// Writes a `Data` message straight from a borrowed payload.
    pub fn write_data(w: &mut impl Write, source: &str, output: &str, payload: &[u8]) -> io::Result<()> {
        write_frame(w, 201, &[source.as_bytes(), output.as_bytes(), payload])
    }

    /// `Ok(None)` on a clean end of stream.
    pub fn read_from(r: &mut impl Read) -> io::Result<Option<Self>> {
        let Some((tag, mut f)) = read_frame(r)? else { return Ok(None) };
        let msg = match tag {
            201 => PeerMsg::Data { source: f.string()?, output: f.string()?, payload: f.bytes()? },
            202 => PeerMsg::Closed { node: f.string()? },
            t => return Err(invalid(format!("unknown peer message tag {t}"))),
        };
        Ok(Some(msg))
    }
}

fn write_frame(w: &mut impl Write, tag: u8, fields: &[&[u8]]) -> io::Result<()> {
    let body_len = 1 + fields.iter().map(|f| 4 + f.len()).sum::<usize>();
    // Build the whole frame first: a single write keeps frames from interleaving.
    let mut out = Vec::with_capacity(4 + body_len);
    out.extend_from_slice(&(body_len as u32).to_le_bytes());
    out.push(tag);
    for f in fields {
        out.extend_from_slice(&(f.len() as u32).to_le_bytes());
        out.extend_from_slice(f);
    }
    w.write_all(&out)
}

fn read_frame(r: &mut impl Read) -> io::Result<Option<(u8, Fields)>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        res => res?,
    }
    let len = u32::from_le_bytes(len);
    if len == 0 || len > MAX_FRAME_LEN {
        return Err(invalid(format!("invalid frame length {len}")));
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body)?;
    let tag = body.remove(0);
    Ok(Some((tag, Fields(body))))
}

struct Fields(Vec<u8>);

impl Fields {
    fn bytes(&mut self) -> io::Result<Vec<u8>> {
        let len = match self.0.get(..4) {
            Some(b) => u32::from_le_bytes(b.try_into().unwrap()) as usize,
            None => return Err(invalid("truncated frame")),
        };
        if self.0.len() < 4 + len {
            return Err(invalid("truncated frame"));
        }
        let rest = self.0.split_off(4 + len);
        let field = std::mem::replace(&mut self.0, rest).split_off(4);
        Ok(field)
    }

    fn u32(&mut self) -> io::Result<u32> {
        let b = self.bytes()?;
        Ok(u32::from_le_bytes(b.try_into().map_err(|_| invalid("expected a u32"))?))
    }

    fn u64(&mut self) -> io::Result<u64> {
        let b = self.bytes()?;
        Ok(u64::from_le_bytes(b.try_into().map_err(|_| invalid("expected a u64"))?))
    }

    fn string(&mut self) -> io::Result<String> {
        String::from_utf8(self.bytes()?).map_err(|_| invalid("string is not valid UTF-8"))
    }
}

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let node = [
            NodeMsg::Register { node_id: "talker".into() },
            NodeMsg::Output { output_id: "count".into(), slot: 3, len: 1 << 40 },
        ];
        let daemon = [
            DaemonMsg::Ready,
            DaemonMsg::Input { input_id: "count".into(), source: "talker".into(), slot: 0, len: 0 },
            DaemonMsg::Stop,
        ];
        let mut wire = Vec::new();
        node.iter().for_each(|m| m.write_to(&mut wire).unwrap());
        let mut r = wire.as_slice();
        for m in &node {
            assert_eq!(NodeMsg::read_from(&mut r).unwrap().as_ref(), Some(m));
        }
        assert_eq!(NodeMsg::read_from(&mut r).unwrap(), None);

        let mut wire = Vec::new();
        daemon.iter().for_each(|m| m.write_to(&mut wire).unwrap());
        let mut r = wire.as_slice();
        for m in &daemon {
            assert_eq!(DaemonMsg::read_from(&mut r).unwrap().as_ref(), Some(m));
        }

        let peer = [
            PeerMsg::Data { source: "camera".into(), output: "frames".into(), payload: vec![7; 1000] },
            PeerMsg::Closed { node: "camera".into() },
        ];
        let mut wire = Vec::new();
        peer.iter().for_each(|m| m.write_to(&mut wire).unwrap());
        let mut r = wire.as_slice();
        for m in &peer {
            assert_eq!(PeerMsg::read_from(&mut r).unwrap().as_ref(), Some(m));
        }
        assert_eq!(PeerMsg::read_from(&mut r).unwrap(), None);
    }
}
