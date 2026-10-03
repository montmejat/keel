//! Wire protocols: daemon <-> nodes over a Unix socket, and daemon <-> daemon
//! over TCP.
//!
//! Local messages don't use the node socket (see [`crate::channel`]): a node
//! registers on it, gets its routes back, and keeps it open so that the
//! daemon notices when it exits.
//!
//! Every frame is `[u32 LE length][u8 tag][fields]`; each field is
//! `[u32 LE length][bytes]`. Hand-rolled and dependency-free on purpose.

use std::io::{self, Read, Write};

use crate::trace::Context;

/// Set by the daemon on every node it spawns.
pub const ENV_NODE_ID: &str = "KEEL_NODE_ID";
pub const ENV_DAEMON_SOCKET: &str = "KEEL_DAEMON_SOCKET";
/// Directory holding the shared-memory regions, see [`crate::shm`].
pub const ENV_SHM_DIR: &str = "KEEL_SHM_DIR";
/// Set for nodes the dataflow marks real-time.
pub const ENV_REALTIME: &str = "KEEL_REALTIME";
/// Where in its cycle a node's periodic loops tick, in nanoseconds: see
/// `periodic`.
pub const ENV_PHASE: &str = "KEEL_PHASE_NS";
/// The dataflow's name (its file's stem), for nodes that name things after it.
pub const ENV_DATAFLOW: &str = "KEEL_DATAFLOW";
/// The id of the deployment the node was started from, when there is one.
pub const ENV_DEPLOYMENT: &str = "KEEL_DEPLOYMENT";

const MAX_FRAME_LEN: u32 = 64 * 1024 * 1024;

/// Node -> daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeMsg {
    Register { node_id: String },
}

/// Daemon -> node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonMsg {
    /// All nodes have registered, so no output can be lost. `routes` says
    /// where this node's messages come from and go, one per line:
    ///
    /// ```text
    /// in <input> <source node> <output> read channel <this node>.in.<input>
    /// out <output> <node> <input>       push to <node>.in.<input>
    /// out <output> @daemon              push to the daemon, for other machines
    /// ```
    Ready { routes: String },
}

impl NodeMsg {
    pub fn write_to(&self, w: &mut impl Write) -> io::Result<()> {
        match self {
            NodeMsg::Register { node_id } => write_frame(w, 1, &[node_id.as_bytes()]),
        }
    }

    /// `Ok(None)` on a clean end of stream.
    pub fn read_from(r: &mut impl Read) -> io::Result<Option<Self>> {
        let Some((tag, mut f)) = read_frame(r)? else {
            return Ok(None);
        };
        let msg = match tag {
            1 => NodeMsg::Register { node_id: f.string()? },
            t => return Err(invalid(format!("unknown node message tag {t}"))),
        };
        Ok(Some(msg))
    }
}

impl DaemonMsg {
    pub fn write_to(&self, w: &mut impl Write) -> io::Result<()> {
        match self {
            DaemonMsg::Ready { routes } => write_frame(w, 101, &[routes.as_bytes()]),
        }
    }

    /// `Ok(None)` on a clean end of stream.
    pub fn read_from(r: &mut impl Read) -> io::Result<Option<Self>> {
        let Some((tag, mut f)) = read_frame(r)? else {
            return Ok(None);
        };
        let msg = match tag {
            101 => DaemonMsg::Ready { routes: f.string()? },
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
    /// `source/output` sent a message; its trace context and payload travel
    /// with it. `published_ns` and `stamp_ns` are on the sending machine's
    /// clock.
    Data { source: String, output: String, context: Context, payload: Vec<u8> },
    /// `node` has exited: nothing more will come from it.
    Closed { node: String },
}

impl PeerMsg {
    pub fn write_to(&self, w: &mut impl Write) -> io::Result<()> {
        match self {
            PeerMsg::Data { source, output, context, payload } => Self::write_data(w, source, output, context, payload),
            PeerMsg::Closed { node } => write_frame(w, 202, &[node.as_bytes()]),
        }
    }

    /// Writes a `Data` message straight from a borrowed payload.
    pub fn write_data(
        w: &mut impl Write,
        source: &str,
        output: &str,
        context: &Context,
        payload: &[u8],
    ) -> io::Result<()> {
        write_frame(
            w,
            201,
            &[
                source.as_bytes(),
                output.as_bytes(),
                &context.span.to_le_bytes(),
                &context.trace.to_le_bytes(),
                &context.parent.to_le_bytes(),
                &context.published_ns.to_le_bytes(),
                &(context.sampled as u32).to_le_bytes(),
                &context.stamp_ns.to_le_bytes(),
                payload,
            ],
        )
    }

    /// `Ok(None)` on a clean end of stream.
    pub fn read_from(r: &mut impl Read) -> io::Result<Option<Self>> {
        let Some((tag, mut f)) = read_frame(r)? else { return Ok(None) };
        let msg = match tag {
            201 => PeerMsg::Data {
                source: f.string()?,
                output: f.string()?,
                context: Context {
                    span: f.u64()?,
                    trace: f.u64()?,
                    parent: f.u64()?,
                    published_ns: f.u64()?,
                    sampled: f.u32()? != 0,
                    stamp_ns: f.u64()?,
                },
                payload: f.bytes()?,
            },
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
    let mut tag = [0u8];
    r.read_exact(&mut tag)?;
    let mut body = vec![0u8; len as usize - 1];
    r.read_exact(&mut body)?;
    Ok(Some((tag[0], Fields { buf: body, pos: 0 })))
}

/// Reads fields in order. Each is copied out once: a payload of megabytes
/// shouldn't be moved around for every small field in front of it.
struct Fields {
    buf: Vec<u8>,
    pos: usize,
}

impl Fields {
    fn bytes(&mut self) -> io::Result<Vec<u8>> {
        let len = match self.buf.get(self.pos..self.pos + 4) {
            Some(b) => u32::from_le_bytes(b.try_into().unwrap()) as usize,
            None => return Err(invalid("truncated frame")),
        };
        let start = self.pos + 4;
        if self.buf.len() - start < len {
            return Err(invalid("truncated frame"));
        }
        self.pos = start + len;
        Ok(self.buf[start..start + len].to_vec())
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
        let node = [NodeMsg::Register { node_id: "talker".into() }];
        let daemon = [DaemonMsg::Ready { routes: "in ack listener\nout count listener count\n".into() }];
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
            PeerMsg::Data {
                source: "camera".into(),
                output: "frames".into(),
                context: Context { span: 1, trace: 2, parent: 3, published_ns: 4, stamp_ns: 5, sampled: true },
                payload: vec![7; 1000],
            },
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
