//! A joint's drive, as a simulation needs one: what stands between a
//! command and the torque on the joint. It closes the loop for positions
//! and velocities with its own [`Pid`], as a real drive's firmware does,
//! takes efforts as they are, and has a real drive's habits: commands that
//! take effect late, a limit on its torque, letting go when commands stop
//! coming, and breaking down.
//!
//! It plays a simulated robot's drives ([`crate::sim`]).

use std::collections::VecDeque;
use std::io;

use crate::law::Pid;
use crate::model::{Breakdown, Fault};
use crate::{Command, State};

#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DriveSettings {
    /// The drive's own loop; its `limit` is the most torque it can give.
    pub pid: Pid,
    /// How long a command takes to take effect, in milliseconds.
    pub delay_ms: f64,
    /// Without a command for this long, in milliseconds, it lets go.
    pub timeout_ms: f64,
    pub fault: Option<Fault>,
}

impl Default for DriveSettings {
    fn default() -> Self {
        DriveSettings { pid: Pid::default(), delay_ms: 0.0, timeout_ms: 100.0, fault: None }
    }
}

pub struct Drive {
    settings: DriveSettings,
    pid: Pid,
    /// Commands on their way, and when each takes effect.
    coming: VecDeque<(u64, Command)>,
    /// The command in force, and since when.
    command: Option<(u64, Command)>,
    start: Option<u64>,
}

impl Drive {
    pub fn new(settings: DriveSettings) -> io::Result<Drive> {
        Fault::check(settings.fault, "a drive", &[Breakdown::Dead, Breakdown::Weak])?;
        Ok(Drive { pid: settings.pid, settings, coming: VecDeque::new(), command: None, start: None })
    }

    /// A command, arriving at `now`.
    pub fn command(&mut self, now: u64, command: Command) {
        let at = now + (self.settings.delay_ms * 1e6) as u64;
        self.coming.push_back((at, command));
    }

    /// The torque on the joint at `now`, `dt` seconds after the last call,
    /// for a joint in `state`.
    pub fn torque(&mut self, now: u64, state: State, dt: f64) -> f64 {
        let start = *self.start.get_or_insert(now);
        while let Some(&(at, command)) = self.coming.front().filter(|(at, _)| *at <= now) {
            self.coming.pop_front();
            if self.command.is_none_or(|(_, c)| c.mode != command.mode) {
                self.pid.reset();
            }
            self.command = Some((at, command));
        }
        let Some((since, command)) = self.command else { return 0.0 };
        if now.saturating_sub(since) as f64 > self.settings.timeout_ms * 1e6 {
            self.command = None;
            return 0.0;
        }
        let torque = self.pid.effort(command, state, dt);
        match self.settings.fault.filter(|f| f.active(start, now)).map(|f| f.what) {
            Some(Breakdown::Dead) => 0.0,
            Some(Breakdown::Weak) => torque * 0.2,
            _ => torque,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Pendulum;

    const MS: u64 = 1_000_000;

    #[test]
    fn holds_a_position_and_lets_go_without_commands() {
        let mut drive = Drive::new(DriveSettings::default()).unwrap();
        let mut plant = Pendulum::default();
        for t in 0..3000 {
            if t < 2000 {
                drive.command(t * MS, Command::position(0.5));
            }
            plant.step(drive.torque(t * MS, plant.state(), 0.001), 0.001);
            if t == 1999 {
                assert!((plant.state().position - 0.5).abs() < 0.01, "{:?}", plant.state());
            }
        }
        assert_eq!(drive.torque(3000 * MS, plant.state(), 0.001), 0.0, "let go");
    }

    #[test]
    fn commands_arrive_late() {
        let settings = DriveSettings { delay_ms: 5.0, ..Default::default() };
        let mut drive = Drive::new(settings).unwrap();
        drive.command(0, Command::effort(2.0));
        assert_eq!(drive.torque(4 * MS, State::default(), 0.001), 0.0);
        assert_eq!(drive.torque(5 * MS, State::default(), 0.001), 2.0);
    }

    #[test]
    fn a_dead_drive_gives_nothing() {
        let settings = DriveSettings { fault: Some(Fault::parse("dead@1").unwrap()), ..Default::default() };
        let mut drive = Drive::new(settings).unwrap();
        drive.command(0, Command::effort(2.0));
        assert_eq!(drive.torque(0, State::default(), 0.001), 2.0);
        drive.command(1000 * MS, Command::effort(2.0));
        assert_eq!(drive.torque(1000 * MS, State::default(), 0.001), 0.0);
        assert!(Drive::new(DriveSettings { fault: Some(Fault::parse("dark").unwrap()), ..Default::default() }).is_err());
    }
}
