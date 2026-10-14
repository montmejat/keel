//! The loops, one per kind of device, and the scheduler that runs them.
//!
//! A loop is a [`Tick`]: what to do every period. [`run`] calls each loop
//! when it's due, on a [`Clock`]: the wall clock for a real device's node
//! ([`Wall`]), a simulation's for the simulated ones ([`crate::sim::SimClock`],
//! which steps the world up to each tick). So a driver's node and a
//! simulation with an arm, an IMU and two cameras run the same loops, the
//! first one alone, the second all together.
//!
//! Ticks fall on multiples of their period, shifted by the node's phase, as
//! [`keel::Periodic`]'s do; on the wall clock, ticks missed while busy are
//! skipped rather than run back to back.

use std::io;
use std::time::{Duration, Instant};

use keel::periodic::{self, next_tick};
use keel::trace::now_ns;
use keel::{Event, Node};

use crate::cycle::{self, Answer, Cycle, Misses};
use crate::device::{Camera, Joints, VectorSensor};
use crate::law::Pid;
use crate::{vector, Command, Image, Mode, State};

/// Whether to go on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Go,
    Stop,
}

/// A loop: what to do every period.
pub trait Tick {
    fn period_ns(&self) -> u64;

    /// An input that arrived between ticks.
    fn input(&mut self, _id: &str, _data: &[u8]) {}

    /// The tick due at `now`, on the scheduler's clock.
    fn tick(&mut self, node: &mut Node, now: u64) -> io::Result<Flow>;
}

/// What the scheduler keeps time with.
pub trait Clock {
    fn now(&self) -> u64;

    /// Where the ticks' grid starts: keel's clock's zero, so that loops of
    /// one period tick together whenever they started; a simulation's
    /// start, so that its ticks fall at the same moments of it every run.
    fn epoch(&self) -> u64 {
        0
    }

    /// Brings the time to `t`: waits for it, or simulates up to it.
    fn advance_to(&mut self, t: u64) -> io::Result<()>;
}

/// keel's clock ([`now_ns`]), which a real device keeps time with.
pub struct Wall;

impl Clock for Wall {
    fn now(&self) -> u64 {
        now_ns()
    }

    fn advance_to(&mut self, t: u64) -> io::Result<()> {
        sleep_until(t);
        Ok(())
    }
}

/// Sleeps until `t` on keel's clock (`CLOCK_MONOTONIC`, absolute: no drift).
pub fn sleep_until(t: u64) {
    let deadline = libc::timespec { tv_sec: (t / 1_000_000_000) as _, tv_nsec: (t % 1_000_000_000) as _ };
    // SAFETY: plain syscall; EINTR just means waking early.
    unsafe { libc::clock_nanosleep(libc::CLOCK_MONOTONIC, libc::TIMER_ABSTIME, &deadline, std::ptr::null_mut()) };
}

/// Runs `loops` on `clock` until the dataflow stops or a loop says to.
/// Inputs that arrive between ticks go to every loop's [`Tick::input`].
pub fn run(node: &mut Node, clock: &mut dyn Clock, mut loops: Vec<Box<dyn Tick + '_>>) -> io::Result<()> {
    if loops.is_empty() {
        return Err(io::Error::other("nothing to run: no device"));
    }
    let phase = periodic::phase().as_nanos() as u64;
    let (start, epoch) = (clock.now(), clock.epoch());
    let mut due: Vec<u64> =
        loops.iter().map(|l| epoch + next_tick(start - epoch, l.period_ns().max(1), phase)).collect();
    loop {
        let t = *due.iter().min().unwrap();
        clock.advance_to(t)?;
        while let Some(event) = node.try_next_event()? {
            match event {
                Event::Input { id, data } => loops.iter_mut().for_each(|l| l.input(id, &data)),
                Event::Stop => return Ok(()),
            }
        }
        for (l, due) in loops.iter_mut().zip(&mut due) {
            if *due > t {
                continue;
            }
            if l.tick(node, t)? == Flow::Stop {
                return Ok(());
            }
            let now = clock.now();
            while *due <= now.max(t) {
                *due += l.period_ns().max(1);
            }
        }
    }
}

/// Where a joint's position or velocity loop is closed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LoopAt {
    /// On the drive: its firmware, or a simulated drive. Commands go to it
    /// as they are.
    #[default]
    Drive,
    /// Here, in the joints' loop, every tick: the drive gets efforts.
    Keel,
}

impl std::str::FromStr for LoopAt {
    type Err = io::Error;

    fn from_str(s: &str) -> io::Result<LoopAt> {
        match s {
            "drive" => Ok(LoopAt::Drive),
            "keel" => Ok(LoopAt::Keel),
            other => Err(io::Error::other(format!("`drive` or `keel`, not {other:?}"))),
        }
    }
}

/// How a joints' loop runs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct JointSettings {
    pub hz: f64,
    pub cycle: Cycle,
    /// How long to wait for a state's answer, in [`Cycle::Same`].
    pub deadline: Duration,
    pub loop_at: LoopAt,
    /// The law, when [`LoopAt::Keel`].
    pub pid: Pid,
}

impl Default for JointSettings {
    fn default() -> Self {
        JointSettings {
            hz: 1000.0,
            cycle: Cycle::Same,
            deadline: Duration::from_micros(300),
            loop_at: LoopAt::Drive,
            pid: Pid::default(),
        }
    }
}

impl JointSettings {
    /// From the command line: `--hz --cycle --deadline-us --loop --kp --ki
    /// --kd --limit`, `defaults` for what isn't there.
    pub fn from_args(defaults: JointSettings) -> io::Result<JointSettings> {
        let p = defaults.pid;
        Ok(JointSettings {
            hz: crate::arg("hz", defaults.hz)?,
            cycle: cycle::cycle_arg()?,
            deadline: Duration::from_micros(crate::arg("deadline-us", defaults.deadline.as_micros() as u64)?),
            loop_at: crate::arg("loop", defaults.loop_at)?,
            pid: Pid::new(
                crate::arg("kp", p.kp)?,
                crate::arg("ki", p.ki)?,
                crate::arg("kd", p.kd)?,
                crate::arg("limit", p.limit)?,
            ),
        })
    }
}

/// The joints' loop, for any [`Joints`]: once a period, publishes `state`,
/// waits for the `command` that answers it (see [`crate::cycle`]), and
/// writes it, closing the loop first if it's [`LoopAt::Keel`]'s to close.
pub struct JointLoop<J: Joints> {
    joints: J,
    settings: JointSettings,
    controlled: bool,
    states: Vec<State>,
    /// What the controller asks for, and what goes to the joints.
    asked: Vec<Command>,
    out: Vec<Command>,
    pids: Vec<Pid>,
    misses: Misses,
    /// When the joints last gave a whole state, and whether their silence
    /// has been mentioned.
    heard: Instant,
    complained: bool,
}

/// Joints silent for this long get a line in the log.
const SILENCE: Duration = Duration::from_secs(1);

impl<J: Joints> JointLoop<J> {
    /// `node` tells whether there's a controller: a `command` input.
    pub fn new(joints: J, mut settings: JointSettings, node: &Node) -> io::Result<Self> {
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
        let n = joints.count();
        Ok(JointLoop {
            joints,
            settings,
            controlled,
            states: vec![State::default(); n],
            asked: vec![Command::default(); n],
            out: vec![Command::default(); n],
            pids: vec![settings.pid; n],
            misses: Misses::new(settings.deadline),
            heard: Instant::now(),
            complained: false,
        })
    }

    /// A law per joint, for [`LoopAt::Keel`], instead of `settings.pid`
    /// for all: joints differ.
    pub fn with_laws(mut self, pids: Vec<Pid>) -> Self {
        assert_eq!(pids.len(), self.pids.len(), "a law per joint");
        self.pids = pids;
        self
    }

    fn take(asked: &mut [Command], data: &[u8]) {
        asked.iter_mut().zip(Command::read(data)).for_each(|(c, new)| *c = new);
    }

    /// What goes to the joints for what was asked.
    fn close_loops(&mut self) -> io::Result<()> {
        let dt = 1.0 / self.settings.hz;
        for (j, ((asked, out), pid)) in self.asked.iter().zip(&mut self.out).zip(&mut self.pids).enumerate() {
            *out = match (self.settings.loop_at, asked.mode) {
                (_, Mode::Effort) => *asked,
                (LoopAt::Keel, _) => Command::effort(pid.effort(*asked, self.states[j], dt)),
                (LoopAt::Drive, mode) if self.joints.closes(mode) => *asked,
                (LoopAt::Drive, mode) => {
                    return Err(io::Error::other(format!(
                        "joint {} is asked for a {mode:?} but its drive only takes efforts: close the loop in keel (`--loop keel`)",
                        j + 1
                    )))
                }
            };
        }
        Ok(())
    }
}

impl<J: Joints> Tick for JointLoop<J> {
    fn period_ns(&self) -> u64 {
        (1e9 / self.settings.hz) as u64
    }

    fn input(&mut self, _id: &str, data: &[u8]) {
        Self::take(&mut self.asked, data);
    }

    fn tick(&mut self, node: &mut Node, _now: u64) -> io::Result<Flow> {
        let Some(stamp) = self.joints.read(&mut self.states)? else {
            if !self.complained && self.heard.elapsed() >= SILENCE {
                self.complained = true;
                println!("no state from every joint for {SILENCE:?}");
            }
            return Ok(Flow::Go);
        };
        if self.complained {
            println!("the joints are back");
        }
        (self.heard, self.complained) = (Instant::now(), false);
        let n = self.states.len();
        node.send_stamped("state", stamp, n * State::LEN, |payload| State::write(&self.states, payload))?;
        if self.controlled && self.settings.cycle != Cycle::Next {
            let asked = node.last_sent_span();
            let timeout = (self.settings.cycle == Cycle::Same).then_some(self.settings.deadline);
            let answer = cycle::wait_for_answer(node, asked, timeout, |data| {
                Self::take(&mut self.asked, data);
                Ok(())
            })?;
            if answer == Answer::Stop {
                return Ok(Flow::Stop);
            }
            self.misses.count(answer);
        }
        self.close_loops()?;
        self.joints.write(&self.out)?;
        Ok(Flow::Go)
    }
}

/// A numeric sensor's loop: once a period, its reading on `output`.
pub struct SensorLoop<S: VectorSensor> {
    sensor: S,
    period: u64,
    output: String,
    values: Vec<f64>,
}

impl<S: VectorSensor> SensorLoop<S> {
    pub fn new(sensor: S, hz: f64, output: &str) -> Self {
        let values = vec![0.0; sensor.count()];
        SensorLoop { sensor, period: (1e9 / hz) as u64, output: output.into(), values }
    }
}

impl<S: VectorSensor> Tick for SensorLoop<S> {
    fn period_ns(&self) -> u64 {
        self.period
    }

    fn tick(&mut self, node: &mut Node, _now: u64) -> io::Result<Flow> {
        if let Some(stamp) = self.sensor.read(&mut self.values)? {
            let values = &self.values;
            node.send_stamped(&self.output, stamp, values.len() * 8, |payload| vector::write(values, payload))?;
        }
        Ok(Flow::Go)
    }
}

/// A camera's loop: once a period, its picture on `output`.
pub struct CameraLoop<C: Camera> {
    camera: C,
    period: u64,
    output: String,
    image: Image,
}

impl<C: Camera> CameraLoop<C> {
    pub fn new(camera: C, hz: f64, output: &str) -> Self {
        CameraLoop { camera, period: (1e9 / hz) as u64, output: output.into(), image: Image::default() }
    }
}

impl<C: Camera> Tick for CameraLoop<C> {
    fn period_ns(&self) -> u64 {
        self.period
    }

    fn tick(&mut self, node: &mut Node, _now: u64) -> io::Result<Flow> {
        if let Some(stamp) = self.camera.capture(&mut self.image)? {
            let image = &self.image;
            node.send_stamped(&self.output, stamp, image.len(), |payload| image.write(payload))?;
        }
        Ok(Flow::Go)
    }
}
