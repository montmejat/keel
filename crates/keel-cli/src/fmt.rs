//! Human-readable numbers.

use std::time::Duration;

/// `42s`, `3m07s`, `2h05m`
pub fn duration(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3600, s / 60 % 60),
    }
}

/// How long ago a Unix time was: `42s ago`, `3m07s ago`
pub fn age(unix_secs: u64) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    format!("{} ago", duration(Duration::from_secs(now.saturating_sub(unix_secs))))
}

/// Time since the daemon started: `+12.345s`
pub fn timestamp(t_ms: u64) -> String {
    format!("+{}.{:03}s", t_ms / 1000, t_ms % 1000)
}

/// `512 B`, `4.0 KiB`, `8.0 MiB`
pub fn bytes(n: f64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut n = n;
    let mut unit = 0;
    while n >= 1024.0 && unit < UNITS.len() - 1 {
        n /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n:.0} {}", UNITS[0])
    } else {
        format!("{n:.1} {}", UNITS[unit])
    }
}

/// A duration in nanoseconds: `850ns`, `12.3µs`, `4.56ms`, `1.23s`
pub fn nanos(ns: u64) -> String {
    match ns {
        0..1_000 => format!("{ns}ns"),
        1_000..1_000_000 => format!("{:.1}µs", ns as f64 / 1e3),
        1_000_000..1_000_000_000 => format!("{:.2}ms", ns as f64 / 1e6),
        _ => format!("{:.2}s", ns as f64 / 1e9),
    }
}

/// `0`, `29.9`, `1.2k`, `340k`
pub fn rate(n: f64) -> String {
    match n {
        n if n < 0.05 => "0".into(),
        n if n < 100.0 => format!("{n:.1}"),
        n if n < 1000.0 => format!("{n:.0}"),
        n if n < 100_000.0 => format!("{:.1}k", n / 1000.0),
        n => format!("{:.0}k", n / 1000.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(duration(Duration::from_secs(187)), "3m07s");
        assert_eq!(duration(Duration::from_secs(7500)), "2h05m");
        assert_eq!(timestamp(12_045), "+12.045s");
        assert_eq!(bytes(512.0), "512 B");
        assert_eq!(bytes(6_220_800.0), "5.9 MiB");
        assert_eq!(rate(29.94), "29.9");
        assert_eq!(rate(342_654.0), "343k");
        assert_eq!(nanos(850), "850ns");
        assert_eq!(nanos(12_345), "12.3µs");
        assert_eq!(nanos(4_561_000), "4.56ms");
    }
}
