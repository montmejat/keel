//! Closing a loop within one cycle. A node that publishes a measurement (a
//! bus master, a simulation) waits, up to a deadline, for the command that
//! answers it, and acts on it in the same cycle instead of the next one: the
//! command follows the state by a hop and the controller's work, not by a
//! period.
//!
//! The answer is recognised by its trace context: a controller that sends
//! while it holds the state names that state as its parent.

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

/// Waits up to `timeout` for an input caused by the message `asked` (see
/// [`Node::last_sent_span`]). Every input that arrives meanwhile goes to
/// `take`, answer or not: a command answering an earlier state is late, but
/// still the newest there is.
pub fn wait_for_answer(
    node: &mut Node,
    asked: Option<u64>,
    timeout: Duration,
    mut take: impl FnMut(&[u8]) -> io::Result<()>,
) -> io::Result<Answer> {
    let deadline = Instant::now() + timeout;
    loop {
        match node.next_event_timeout(deadline.saturating_duration_since(Instant::now()))? {
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

/// `--cycle same|next`: whether a command acts in the cycle of the state it
/// answers, or in the next one.
pub fn same_cycle_arg() -> io::Result<bool> {
    match crate::arg("cycle", String::from("same"))?.as_str() {
        "same" => Ok(true),
        "next" => Ok(false),
        other => Err(io::Error::other(format!("--cycle: `same` or `next`, not {other:?}"))),
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
