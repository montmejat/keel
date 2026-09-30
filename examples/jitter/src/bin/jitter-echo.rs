//! Replies to every message from `jitter-ticker`.

use keel::{Event, Node};

fn main() -> std::io::Result<()> {
    let mut node = Node::from_env()?;
    while let Event::Input { data, .. } = node.next_event()? {
        node.send_output("pong", &data)?;
    }
    Ok(())
}
