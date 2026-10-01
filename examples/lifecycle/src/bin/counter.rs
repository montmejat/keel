//! Counts at 100 Hz, for 15 s.

use std::time::Duration;

use keel::{Node, Periodic};

fn main() -> std::io::Result<()> {
    let mut node = Node::from_env()?;
    let mut periodic = Periodic::new(Duration::from_millis(10));
    for n in 0..1500u64 {
        periodic.wait();
        node.send_output("count", &n.to_le_bytes())?;
    }
    println!("sent 1500 counts");
    Ok(())
}
