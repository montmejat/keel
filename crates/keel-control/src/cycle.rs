//! Closing a loop within one cycle. A node that publishes a measurement (a
//! bus master, a simulation) waits, up to a deadline, for the command that
//! answers it, and acts on it in the same cycle instead of the next one: the
//! command follows the state by a hop and the controller's work, not by a
//! period.
//!
//! The answer is recognised by its trace context: a controller that sends
//! while it holds the state names that state as its parent.
//!
//! With no deadline, and no clock either, this is lockstep: a simulation
//! steps when its controller has answered, faster or slower than real time.
//! Controllers can't tell, as long as they take their time steps from the
//! states' stamps (`Sample::stamp_ns`) rather than from the clock.

use std::io;
use std::time::{Duration, Instant};

use keel::{Event, Node};

/// How a wait for an answer ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    /// The command caused by the state arrived in time.
    InTime,
    /// The deadline passed first.
    Late,
    /// The dataflow is stopping.
    Stop,
}

/// Waits up to `timeout` (`None`: for as long as it takes) for an input
/// caused by the message `asked` (see [`Node::last_sent_span`]). Every input
/// that arrives meanwhile goes to `take`, answer or not: a command answering
/// an earlier state is late, but still the newest there is.
pub fn wait_for_answer(
    node: &mut Node,
    asked: Option<u64>,
    timeout: Option<Duration>,
    mut take: impl FnMut(&[u8]) -> io::Result<()>,
) -> io::Result<Answer> {
    let deadline = timeout.map(|t| Instant::now() + t);
    loop {
        let event = match deadline {
            Some(d) => node.next_event_timeout(d.saturating_duration_since(Instant::now()))?,
            None => Some(node.next_event()?),
        };
        match event {
            Some(Event::Input { data, .. }) => {
                take(&data)?;
                if asked.is_some_and(|span| data.context().parent == span) {
                    return Ok(Answer::InTime);
                }
            }
            Some(Event::Stop) => return Ok(Answer::Stop),
            None => return Ok(Answer::Late),
        }
    }
}

/// When a command acts, relative to the state it answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cycle {
    /// In the same cycle, if it comes before the deadline.
    Same,
    /// At the next tick: a constant one-period delay.
    Next,
    /// When it comes: no deadline, no clock. Only for simulations.
    Lockstep,
}

/// `--cycle same|next|lockstep`, `same` by default.
pub fn cycle_arg() -> io::Result<Cycle> {
    match crate::arg("cycle", String::from("same"))?.as_str() {
        "same" => Ok(Cycle::Same),
        "next" => Ok(Cycle::Next),
        "lockstep" => Ok(Cycle::Lockstep),
        other => Err(io::Error::other(format!("--cycle: `same`, `next` or `lockstep`, not {other:?}"))),
    }
}

/// Counts answers that missed their deadline, and says so in the log once a
/// second while there are any.
pub struct Misses {
    deadline: Duration,
    cycles: u64,
    missed: u64,
    since: Instant,
}

impl Misses {
    pub fn new(deadline: Duration) -> Self {
        Self { deadline, cycles: 0, missed: 0, since: Instant::now() }
    }

    pub fn count(&mut self, answer: Answer) {
        self.cycles += 1;
        self.missed += (answer == Answer::Late) as u64;
        if self.since.elapsed() >= Duration::from_secs(1) {
            if self.missed > 0 {
                println!(
                    "{} of {} commands missed the {:?} deadline: those joints kept the previous one",
                    self.missed, self.cycles, self.deadline
                );
            }
            (self.cycles, self.missed, self.since) = (0, 0, Instant::now());
        }
    }
}
