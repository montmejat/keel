//! Periodic loops, the shape of a controller: wake up on absolute deadlines,
//! so that the time spent working doesn't add up as drift.

use std::time::Duration;

use crate::trace::now_ns;

pub struct Periodic {
    period: u64,
    next: u64,
    missed: u64,
}

impl Periodic {
    /// The first deadline is one period from now.
    pub fn new(period: Duration) -> Self {
        let period = period.as_nanos() as u64;
        Self { period, next: now_ns() + period, missed: 0 }
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
