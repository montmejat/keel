//! A reading at 100 Hz, until stopped.

use std::time::Duration;

use keel::{Node, Periodic};

fn main() -> std::io::Result<()> {
    let mut node = Node::from_env()?;
    let mut periodic = Periodic::new(Duration::from_millis(10));
    for n in 0u64.. {
        periodic.wait();
        node.send_output("reading", &n.to_le_bytes())?;
    }
    Ok(())
}
