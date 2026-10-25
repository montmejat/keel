//! Joints, and the one loop that runs any of them.
//!
//! A driver is what its joints need: read every joint's state, write every
//! joint's command, and whatever starting and stopping they take ([`Joints`]).
//! The loop ([`JointLoop`]) is the rest, the same for a bus master and a
//! simulation: once a period, publish the state, wait for the command that
//! answers it (see [`crate::cycle`]), write it; log the commands that came
//! late and the joints that went quiet. And watch each joint, what it's told
//! against what it does ([`crate::health`]): what's wrong goes in the state's
//! status, for whoever reads it, and in the log.

use std::io;
use std::time::{Duration, Instant};

use keel::{Event, Node, Periodic};

use crate::cycle::{self, Answer, Cycle, Misses};
use crate::health::{self, Watch};
use crate::{Command, State};

/// Joints a loop can run: on a bus, simulated.
pub trait Joints {
    fn count(&self) -> usize;

    /// For `keel doctor`: whatever is worth checking before a run, reported
    /// with [`keel::check::report`]. Opening the joints already checked that
    /// they can be opened.
    fn check(&mut self) -> io::Result<()> {
        Ok(())
    }

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
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub hz: f64,
    pub cycle: Cycle,
    /// How long to wait for a state's answer, in [`Cycle::Same`].
    pub deadline: Duration,
    /// When a joint isn't doing what it's told.
    pub health: health::Settings,
    /// What to call each joint in the log, after its number.
    pub names: Vec<String>,
}

impl Settings {
    /// From the command line: `--hz 1000 --cycle same --deadline-us 300`,
    /// and for watching the joints `--names a,b --error-rad 0.2 --ratio 0.3
    /// --min-asked 1 --hold-s 0.5 --grace-s 2 --summary-s 5`.
    pub fn from_args() -> io::Result<Settings> {
        let d = health::Settings::default();
        let names: String = crate::arg("names", String::new())?;
        Ok(Settings {
            hz: crate::arg("hz", 1000.0)?,
            cycle: cycle::cycle_arg()?,
            deadline: Duration::from_micros(crate::arg("deadline-us", 300)?),
            health: health::Settings {
                error: crate::arg("error-rad", d.error)?,
                ratio: crate::arg("ratio", d.ratio)?,
                min_asked: crate::arg("min-asked", d.min_asked)?,
                hold: crate::arg("hold-s", d.hold)?,
                moved: d.moved,
                grace: crate::arg("grace-s", d.grace)?,
                summary: crate::arg("summary-s", d.summary)?,
            },
            names: names.split(',').map(|n| n.trim().to_owned()).filter(|n| !n.is_empty()).collect(),
        })
    }
}

/// A joints node's `main`, after opening its joints: checks them if
/// `keel doctor` asked, joins the dataflow, and runs the loop, waiting with
/// `--spin-us` (see `Node::set_spin`).
pub fn run(mut joints: impl Joints, settings: Settings) -> io::Result<()> {
    if keel::check::requested() {
        joints.check()?;
    }
    let mut node = Node::from_env()?;
    node.set_spin(Duration::from_micros(crate::arg("spin-us", 0)?));
    JointLoop::new(joints, settings, &node)?.run(&mut node)
}

/// Prints a node's error as a sentence, not as a debug dump, and exits
/// with a failure: for `main`.
pub fn exit_on(result: io::Result<()>) -> std::process::ExitCode {
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            std::process::ExitCode::FAILURE
        }
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
        let Settings { hz, cycle, deadline, .. } = self.settings;
        let mut watch = Watch::new(self.settings.health, self.settings.names.clone());
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
                let told = if commanded { &commands[..] } else { &[] };
                for line in watch.observe(stamp as f64 / 1e9, &mut states, told) {
                    println!("{line}");
                }
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
