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
use std::time::Duration;

use jitter::MESSAGE_LEN;
use keel::{Event, Node};

const PERIOD: Duration = Duration::from_millis(1);
const TICKS: usize = 10_000;

fn main() -> io::Result<()> {
    let mut node = Node::from_env()?;
    let mut lateness = Vec::with_capacity(TICKS);
    let mut rtts = Vec::with_capacity(TICKS);
    let mut missed = 0;

    let mut deadline = now() + PERIOD;
    for _ in 0..TICKS {
        sleep_until(deadline);
        let woke = now();
        lateness.push(woke.saturating_sub(deadline));

        node.send_with("ping", MESSAGE_LEN, |buf| buf[0] = 0)?;
        match node.next_event()? {
            Event::Input { .. } => rtts.push(now() - woke),
            Event::Stop => break,
        }

        deadline += PERIOD;
        while deadline < now() {
            deadline += PERIOD;
            missed += 1;
        }
    }

    println!("{} ticks at {:?}, {missed} deadlines missed", lateness.len(), PERIOD);
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

/// `CLOCK_MONOTONIC`, the clock `clock_nanosleep` sleeps on below.
fn now() -> Duration {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: plain syscall (vDSO) writing into a local.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

/// Sleeps until an absolute time, so that the time spent computing the next
/// deadline doesn't add up as drift.
fn sleep_until(deadline: Duration) {
    let ts = libc::timespec { tv_sec: deadline.as_secs() as _, tv_nsec: deadline.subsec_nanos() as _ };
    // SAFETY: plain syscall; EINTR just means waking early, and the caller
    // measures the actual wake-up time anyway.
    unsafe { libc::clock_nanosleep(libc::CLOCK_MONOTONIC, libc::TIMER_ABSTIME, &ts, std::ptr::null_mut()) };
}
