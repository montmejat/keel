//! keel node API.
//!
//! A node is a process spawned by the daemon. It connects back over the Unix
//! socket named in its environment, publishes on its outputs and receives
//! events on its inputs.

pub mod protocol;

use std::io;
use std::os::unix::net::UnixStream;

use protocol::{DaemonMsg, NodeMsg, ENV_DAEMON_SOCKET, ENV_NODE_ID};

pub enum Event {
    Input { id: String, data: Vec<u8> },
    /// All upstream nodes have exited; the node should return.
    Stop,
}

pub struct Node {
    id: String,
    stream: UnixStream,
}

impl Node {
    /// Connects to the daemon that spawned this process. Blocks until every
    /// node in the dataflow has registered.
    pub fn from_env() -> io::Result<Self> {
        let id = env(ENV_NODE_ID)?;
        let mut stream = UnixStream::connect(env(ENV_DAEMON_SOCKET)?)?;
        NodeMsg::Register { node_id: id.clone() }.write_to(&mut stream)?;
        match DaemonMsg::read_from(&mut stream)? {
            Some(DaemonMsg::Ready) => Ok(Self { id, stream }),
            other => Err(io::Error::other(format!("expected Ready from daemon, got {other:?}"))),
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn send_output(&mut self, output_id: &str, data: &[u8]) -> io::Result<()> {
        NodeMsg::Output { output_id: output_id.to_owned(), data: data.to_vec() }.write_to(&mut self.stream)
    }

    /// Blocks until the next event. A closed connection is reported as `Stop`.
    pub fn next_event(&mut self) -> io::Result<Event> {
        match DaemonMsg::read_from(&mut self.stream)? {
            Some(DaemonMsg::Input { input_id, data }) => Ok(Event::Input { id: input_id, data }),
            Some(DaemonMsg::Stop) | None => Ok(Event::Stop),
            Some(other) => Err(io::Error::other(format!("unexpected message from daemon: {other:?}"))),
        }
    }
}

fn env(name: &str) -> io::Result<String> {
    std::env::var(name)
        .map_err(|_| io::Error::other(format!("{name} is not set; nodes must be started by keel-daemon")))
}
