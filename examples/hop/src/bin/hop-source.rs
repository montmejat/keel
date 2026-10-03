//! Publishes `--count` messages of `--size` bytes, one every `--period-us`.
//! Only the first byte is written: this measures the hop, not filling a
//! buffer. Prints how long `send_with` took: the sender's share of a hop.

use std::time::Duration;

use hop::{arg, report};
use keel::trace::now_ns;
use keel::{Node, Periodic};

fn main() -> std::io::Result<()> {
    let size: usize = arg("size", 64);
    let count: usize = arg("count", 20_000);
    let period = Duration::from_micros(arg("period-us", 1000));
    let mut node = Node::from_env()?;
    let mut periodic = Periodic::new(period);
    let mut sends = Vec::with_capacity(count);
    for _ in 0..count {
        periodic.wait();
        let start = now_ns();
        node.send_with("data", size, |buf| buf[0] = 1)?;
        sends.push(now_ns() - start);
    }
    report("source, send_with", &mut sends);
    Ok(())
}
