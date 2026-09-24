//! Echoes pings and acknowledges the end of each bulk burst.

use bench::{BULK_LAST, PING};
use keel::{Event, Node};

fn main() -> std::io::Result<()> {
    let mut node = Node::from_env()?;
    while let Event::Input { data, .. } = node.next_event()? {
        match data.first() {
            Some(&PING) | Some(&BULK_LAST) => node.send_output("reply", &data[..1])?,
            _ => {}
        }
    }
    Ok(())
}
