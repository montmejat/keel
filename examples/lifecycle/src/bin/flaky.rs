//! Forwards counts, and crashes on every 250th: `restart: on-failure` brings
//! it back, and the counts sent meanwhile wait in its channel. The count it
//! crashed on is lost: it took it and never sent it on.

use keel::{Event, Node};
use lifecycle::count;

fn main() -> std::io::Result<()> {
    let mut node = Node::from_env()?;
    while let Event::Input { data, .. } = node.next_event()? {
        let n = count(&data);
        if n > 0 && n.is_multiple_of(250) {
            eprintln!("crashing at {n}");
            std::process::exit(1);
        }
        node.send_output("count", &data)?;
    }
    Ok(())
}
