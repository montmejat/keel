//! Joints, and the one loop that runs any of them.
//!
//! A driver is what its joints need: read every joint's state, write every
//! joint's command, and whatever starting and stopping they take ([`Joints`]).
//! The loop ([`JointLoop`]) is the rest, the same for a bus master and a
//! simulation: once a period, publish the state, wait for the command that
//! answers it (see [`crate::cycle`]), write it; log the commands that came
//! late and the joints that went quiet.

use std::io;
use std::time::{Duration, Instant};

use keel::{Event, Node, Periodic};

use crate::cycle::{self, Answer, Cycle, Misses};
use crate::{Command, State};

/// Joints a loop can run: on a bus, simulated.
pub trait Joints {
    fn count(&self) -> usize;

    /// Before the first cycle: put the drives in their mode, enable them.
    fn start(&mut self) -> io::Result<()> {
        Ok(())
    }

    /// Every joint's state, and the moment it describes (keel's clock, or
    /// a simulation's); `None` until there's a whole one since the last.
    fn read(&mut self, states: &mut [State]) -> io::Result<Option<u64>>;

    /// A cycle's commands: the newest there are, every cycle, whether or
    /// not there was a state; `None` until the controller has sent one.
    /// `states` are the latest read, for joints that close a command's loop
    /// themselves.
    fn write(&mut self, commands: Option<&[Command]>, states: &[State]) -> io::Result<()>;

    /// After the last cycle: disable the drives.
    fn stop(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// How a joints' loop runs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Settings {
    pub hz: f64,
    pub cycle: Cycle,
    /// How long to wait for a state's answer, in [`Cycle::Same`].
    pub deadline: Duration,
}

impl Settings {
    /// From the command line: `--hz 1000 --cycle same --deadline-us 300`.
    pub fn from_args() -> io::Result<Settings> {
        Ok(Settings {
            hz: crate::arg("hz", 1000.0)?,
            cycle: cycle::cycle_arg()?,
            deadline: Duration::from_micros(crate::arg("deadline-us", 300)?),
        })
    }
}

/// Joints silent for this long get a line in the log.
const SILENCE: Duration = Duration::from_secs(1);

/// The loop over any [`Joints`].
pub struct JointLoop<J: Joints> {
    joints: J,
    settings: Settings,
    /// Whether there's a controller: a `command` input.
    controlled: bool,
}

impl<J: Joints> JointLoop<J> {
    pub fn new(joints: J, mut settings: Settings, node: &Node) -> io::Result<Self> {
        let controlled = !node.inputs().is_empty();
        if settings.cycle == Cycle::Lockstep && !controlled {
            return Err(io::Error::other(
                "--cycle lockstep steps when the controller answers: it needs a `command` input",
            ));
        }
        // Nothing to wait for without a controller.
        if !controlled {
            settings.cycle = Cycle::Next;
        }
        Ok(JointLoop { joints, settings, controlled })
    }

    /// Starts the joints, runs cycles until the dataflow stops, and stops
    /// the joints, whatever ended the cycles.
    pub fn run(mut self, node: &mut Node) -> io::Result<()> {
        self.joints.start()?;
        let ran = self.cycles(node);
        let stopped = self.joints.stop();
        ran.and(stopped)
    }

    fn cycles(&mut self, node: &mut Node) -> io::Result<()> {
        let Settings { hz, cycle, deadline } = self.settings;
        let n = self.joints.count();
        let (mut states, mut commands) = (vec![State::default(); n], vec![Command::default(); n]);
        // Whether the controller has sent anything yet.
        let mut commanded = false;
        let mut misses = Misses::new(deadline);
        let (mut heard, mut complained) = (Instant::now(), false);
        let mut periodic = Periodic::new(Duration::from_secs_f64(1.0 / hz));
        loop {
            if cycle != Cycle::Lockstep {
                periodic.wait();
            }
            // Commands that came between ticks: late, or every one in
            // next-cycle mode. The newest is written this cycle.
            while let Some(event) = node.try_next_event()? {
                match event {
                    Event::Input { data, .. } => commanded = take(&mut commands, &data),
                    Event::Stop => return Ok(()),
                }
            }
            if let Some(stamp) = self.joints.read(&mut states)? {
                if complained {
                    println!("the joints are back");
                }
                (heard, complained) = (Instant::now(), false);
                node.send_stamped("state", stamp, n * State::LEN, |payload| State::write(&states, payload))?;
                if self.controlled && cycle != Cycle::Next {
                    let asked = node.last_sent_span();
                    let timeout = (cycle == Cycle::Same).then_some(deadline);
                    let answer = cycle::wait_for_answer(node, asked, timeout, |data| {
                        commanded = take(&mut commands, data);
                        Ok(())
                    })?;
                    if answer == Answer::Stop {
                        return Ok(());
                    }
                    if cycle == Cycle::Same {
                        misses.count(answer);
                    }
                }
            } else if !complained && heard.elapsed() >= SILENCE {
                complained = true;
                println!("no state from every joint for {SILENCE:?}");
            }
            self.joints.write(commanded.then_some(&commands[..]), &states)?;
        }
    }
}

/// Keeps the newest commands; true, since there are some now.
fn take(commands: &mut [Command], data: &[u8]) -> bool {
    commands.iter_mut().zip(Command::read(data)).for_each(|(c, new)| *c = new);
    true
}
