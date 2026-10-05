//! Takes the filtered readings and says how many it has had, every 5 s.

use std::time::{Duration, Instant};

use keel::{Event, Node};

fn main() -> std::io::Result<()> {
    let mut node = Node::from_env()?;
    let (mut count, mut said) = (0u64, Instant::now());
    while let Event::Input { .. } = node.next_event()? {
        count += 1;
        if said.elapsed() >= Duration::from_secs(5) {
            said = Instant::now();
            println!("{count} readings so far");
        }
    }
    Ok(())
}
