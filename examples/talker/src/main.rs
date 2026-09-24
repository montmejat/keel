//! Sends a few counts and waits for each to be acknowledged.

use keel::{Event, Node};

fn main() -> std::io::Result<()> {
    let mut node = Node::from_env()?;
    for i in 0..5 {
        println!("sending count {i}");
        node.send_output("count", i.to_string().as_bytes())?;
        match node.next_event()? {
            Event::Input { id, data } => {
                println!("{id}: {}", String::from_utf8_lossy(&data))
            }
            Event::Stop => return Ok(()),
        }
    }
    Ok(())
}
