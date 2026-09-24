//! Acknowledges every count it receives, until the talker exits.

use keel::{Event, Node};

fn main() -> std::io::Result<()> {
    let mut node = Node::from_env()?;
    while let Event::Input { id, data } = node.next_event()? {
        let text = String::from_utf8_lossy(&data);
        println!("[{}] {id}: {text}", node.id());
        node.send_output("ack", format!("got {text}").as_bytes())?;
    }
    println!("[{}] stopping", node.id());
    Ok(())
}
