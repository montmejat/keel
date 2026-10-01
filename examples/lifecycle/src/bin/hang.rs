//! Forwards counts, but gets stuck in a busy loop at 700: its watchdog kills
//! it, and `restart: on-failure` brings it back, past 700.

use keel::{Event, Node};
use lifecycle::count;

fn main() -> std::io::Result<()> {
    let mut node = Node::from_env()?;
    while let Event::Input { data, .. } = node.next_event()? {
        let n = count(&data);
        if n == 700 {
            eprintln!("stuck at {n}");
            #[allow(clippy::empty_loop)]
            loop {}
        }
        node.send_output("count", &data)?;
    }
    Ok(())
}
