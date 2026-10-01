//! Receives counts and says, when it stops, how many it got and which were
//! missing.

use keel::{Event, Node};
use lifecycle::count;

fn main() -> std::io::Result<()> {
    let mut node = Node::from_env()?;
    let mut seen = Vec::new();
    while let Event::Input { data, .. } = node.next_event()? {
        seen.push(count(&data));
    }
    seen.sort();
    seen.dedup();
    let last = seen.last().copied().unwrap_or(0);
    let missing: Vec<u64> = (0..=last).filter(|n| seen.binary_search(n).is_err()).collect();
    println!("received {} distinct counts up to {last}; missing: {missing:?}", seen.len());
    Ok(())
}
