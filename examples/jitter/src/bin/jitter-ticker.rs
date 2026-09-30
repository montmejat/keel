//! A 1 kHz periodic loop, the shape of a control loop: wakes up on an absolute
//! deadline, sends a small message to `jitter-echo` and waits for the reply.
//!
//! Reports two distributions, the worst case included:
//! - how late each wake-up was: the kernel's and the scheduler's share;
//! - the round trip to `jitter-echo`: keel's share.
//!
//! A tick whose round trip overruns the period skips the deadlines it missed
//! rather than bursting to catch up; they're counted as missed.

use std::io;
use std::time::{Duration, Instant};

use jitter::MESSAGE_LEN;
use keel::{Event, Node, Periodic};

const PERIOD: Duration = Duration::from_millis(1);
const TICKS: usize = 10_000;

fn main() -> io::Result<()> {
    let mut node = Node::from_env()?;
    let mut lateness = Vec::with_capacity(TICKS);
    let mut rtts = Vec::with_capacity(TICKS);

    let mut periodic = Periodic::new(PERIOD);
    for _ in 0..TICKS {
        lateness.push(periodic.wait());
        let woke = Instant::now();
        node.send_with("ping", MESSAGE_LEN, |buf| buf[0] = 0)?;
        match node.next_event()? {
            Event::Input { .. } => rtts.push(woke.elapsed()),
            Event::Stop => break,
        }
    }

    println!("{} ticks at {:?}, {} deadlines missed", lateness.len(), PERIOD, periodic.missed());
    println!("{:>10} {:>10} {:>10} {:>10} {:>10}", "", "p50", "p99", "p99.9", "max");
    report("wake-up", &mut lateness);
    report("rtt", &mut rtts);
    Ok(())
}

fn report(name: &str, samples: &mut [Duration]) {
    if samples.is_empty() {
        return;
    }
    samples.sort();
    let at = |q: f64| format!("{:.1?}", samples[((samples.len() - 1) as f64 * q) as usize]);
    println!("{name:>10} {:>10} {:>10} {:>10} {:>10}", at(0.5), at(0.99), at(0.999), at(1.0));
}
