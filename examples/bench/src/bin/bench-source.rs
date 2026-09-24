//! Measures round-trip latency and one-way throughput to `bench-sink`, for a
//! range of message sizes.
//!
//! Payloads are written in place and only their first byte is set, so this
//! measures the middleware, not the cost of filling buffers.

use std::io;
use std::time::{Duration, Instant};

use bench::{BULK, BULK_LAST, PING};
use keel::{Event, Node};

const SIZES: [usize; 5] = [64, 4 << 10, 64 << 10, 1 << 20, 8 << 20];
const PINGS: usize = 1000;
/// Bytes sent per size in the throughput phase.
const BULK_BYTES: usize = 512 << 20;

fn main() -> io::Result<()> {
    let mut node = Node::from_env()?;
    println!("{:>8} {:>10} {:>10} {:>12} {:>10}", "size", "rtt p50", "rtt p99", "throughput", "msg/s");
    for size in SIZES {
        let mut rtts: Vec<Duration> = (0..PINGS)
            .map(|_| {
                let start = Instant::now();
                node.send_with("data", size, |buf| buf[0] = PING)?;
                wait_reply(&mut node)?;
                Ok(start.elapsed())
            })
            .collect::<io::Result<_>>()?;
        rtts.sort();

        let count = (BULK_BYTES / size).clamp(10, 100_000);
        let start = Instant::now();
        for i in 0..count {
            let kind = if i == count - 1 { BULK_LAST } else { BULK };
            node.send_with("data", size, |buf| buf[0] = kind)?;
        }
        wait_reply(&mut node)?;
        let secs = start.elapsed().as_secs_f64();

        println!(
            "{:>8} {:>10} {:>10} {:>9.0} MB/s {:>10.0}",
            human(size),
            format!("{:.1?}", rtts[PINGS / 2]),
            format!("{:.1?}", rtts[PINGS * 99 / 100]),
            (count * size) as f64 / secs / 1e6,
            count as f64 / secs,
        );
    }
    Ok(())
}

fn wait_reply(node: &mut Node) -> io::Result<()> {
    match node.next_event()? {
        Event::Input { .. } => Ok(()),
        Event::Stop => Err(io::Error::other("sink stopped early")),
    }
}

fn human(size: usize) -> String {
    match size {
        s if s >= 1 << 20 => format!("{}MiB", s >> 20),
        s if s >= 1 << 10 => format!("{}KiB", s >> 10),
        s => format!("{s}B"),
    }
}
