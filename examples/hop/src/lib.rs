//! One hop, measured one way: from the moment a message is published to the
//! moment its receiver has it in hand. `hop-source` and `hop-sink` measure
//! it through keel; `hop-floor` measures what the machine allows, with keel
//! out of the way.
//!
//! The source sends at a steady pace, like a control loop, rather than back
//! to back: between messages the receiver is idle, so each one pays the full
//! cost of reaching it, whether it sleeps or spins.

use std::str::FromStr;
use std::time::Duration;

/// Latencies of the first messages are dropped: page faults, caches, CPU
/// frequency ramping up.
pub const WARMUP: usize = 1000;

/// `--name value` from the command line, or `default`.
pub fn arg<T: FromStr>(name: &str, default: T) -> T {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg.strip_prefix("--") == Some(name) {
            let value = args.next().unwrap_or_default();
            return value.parse().unwrap_or_else(|_| panic!("--{name}: can't read {value:?}"));
        }
    }
    default
}

/// `--spin`: `off`, `forever`, or microseconds.
pub fn spin_arg() -> Duration {
    match arg("spin", String::from("off")).as_str() {
        "off" => Duration::ZERO,
        "forever" => Duration::MAX,
        us => Duration::from_micros(us.parse().unwrap_or_else(|_| panic!("--spin: can't read {us:?}"))),
    }
}

/// Prints percentiles of `ns`, after the warm-up.
pub fn report(label: &str, ns: &mut [u64]) {
    let warmup = WARMUP.min(ns.len());
    let ns = &mut ns[warmup..];
    if ns.is_empty() {
        println!("{label}: no samples");
        return;
    }
    ns.sort_unstable();
    let at = |q: f64| ns[((ns.len() - 1) as f64 * q) as usize];
    println!(
        "{label:<28} n={:<6} p50 {:>8}  p90 {:>8}  p99 {:>8}  p99.9 {:>8}  max {:>8}",
        ns.len(),
        human(at(0.5)),
        human(at(0.9)),
        human(at(0.99)),
        human(at(0.999)),
        human(ns[ns.len() - 1]),
    );
}

fn human(ns: u64) -> String {
    match ns {
        n if n >= 1_000_000 => format!("{:.2}ms", n as f64 / 1e6),
        n if n >= 10_000 => format!("{:.1}µs", n as f64 / 1e3),
        n if n >= 1_000 => format!("{:.2}µs", n as f64 / 1e3),
        n => format!("{n}ns"),
    }
}
