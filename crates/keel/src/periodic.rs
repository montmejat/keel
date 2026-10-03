//! Periodic loops, the shape of a controller: wake up on absolute deadlines,
//! so that the time spent working doesn't add up as drift.
//!
//! Deadlines fall on multiples of the period on the machine's monotonic
//! clock, shifted by the node's phase (`phase_us` in the dataflow). So loops
//! of one period tick together on a machine whenever they started, a 20 ms
//! loop ticks with every 20th tick of a 1 ms one, and a phase puts a loop at
//! a chosen point of the cycle: a state request just before a bus master's
//! tick, a policy just after a camera's. Machines' clocks aren't aligned:
//! phases hold within one machine.

use std::time::Duration;

use crate::protocol::ENV_PHASE;
use crate::trace::now_ns;

pub struct Periodic {
    period: u64,
    next: u64,
    missed: u64,
}

impl Periodic {
    /// Ticks every `period`, at the node's phase (see [`phase`]), starting
    /// with the next tick.
    pub fn new(period: Duration) -> Self {
        Self::with_phase(period, phase())
    }

    /// Ticks every `period`, `phase` after each multiple of it.
    pub fn with_phase(period: Duration, phase: Duration) -> Self {
        let period = (period.as_nanos() as u64).max(1);
        Self { period, next: next_tick(now_ns(), period, phase.as_nanos() as u64), missed: 0 }
    }

    /// Sleeps until the next deadline and returns how late the wake-up was.
    /// Deadlines that already passed while the loop was busy are skipped
    /// rather than run back to back, and counted by [`Periodic::missed`].
    pub fn wait(&mut self) -> Duration {
        let now = now_ns();
        while self.next < now {
            self.next += self.period;
            self.missed += 1;
        }
        let deadline =
            libc::timespec { tv_sec: (self.next / 1_000_000_000) as _, tv_nsec: (self.next % 1_000_000_000) as _ };
        // SAFETY: plain syscall; EINTR just means waking early, and the
        // lateness is measured below either way.
        unsafe { libc::clock_nanosleep(libc::CLOCK_MONOTONIC, libc::TIMER_ABSTIME, &deadline, std::ptr::null_mut()) };
        let late = Duration::from_nanos(now_ns().saturating_sub(self.next));
        self.next += self.period;
        late
    }

    /// Deadlines skipped so far.
    pub fn missed(&self) -> u64 {
        self.missed
    }
}

/// The node's phase, set by the daemon from the dataflow's `phase_us`; zero
/// without one.
pub fn phase() -> Duration {
    let ns = std::env::var(ENV_PHASE).ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    Duration::from_nanos(ns)
}

/// The first tick after `now` of a loop ticking `phase` after every multiple
/// of `period`, all in nanoseconds.
pub fn next_tick(now: u64, period: u64, phase: u64) -> u64 {
    let phase = phase % period;
    if now < phase {
        return phase;
    }
    ((now - phase) / period + 1) * period + phase
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticks_fall_on_the_same_grid_whenever_a_loop_starts() {
        let ms = 1_000_000;
        assert_eq!(next_tick(5_200_000, ms, 0), 6 * ms);
        assert_eq!(next_tick(5_900_000, ms, 0), 6 * ms, "a loop started later ticks with it");
        assert_eq!(next_tick(6 * ms, ms, 0), 7 * ms, "after now, never at it");
        assert_eq!(next_tick(5_200_000, ms, 300_000), 5_300_000, "a phase shifts the grid");
        assert_eq!(next_tick(5_400_000, ms, 300_000), 6_300_000);
        assert_eq!(next_tick(5_200_000, 20 * ms, 0), 20 * ms, "slower loops tick with the fast one");
        assert_eq!(next_tick(100, ms, 300_000), 300_000);
        assert_eq!(next_tick(5_200_000, ms, 1_300_000), 5_300_000, "a phase over a period wraps");
    }
}
