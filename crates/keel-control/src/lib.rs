//! Control on top of keel: joints, a controller, and the buses joints are
//! reached through, all as ordinary nodes.
//!
//! The interface is the dataflow, not a trait: whatever publishes `state` and
//! takes `command` is the hardware. `keel-sim` (a simulated joint) and
//! `keel-can` (joints on a CAN bus) both do, so a controller runs against
//! either unchanged, and swapping them is a change to the dataflow file.
//!
//! Inside such a node, a driver is only what its joints need ([`joints::Joints`]:
//! read, write, start, stop), and one loop runs any of them
//! ([`joints::JointLoop`]): [`damiao::DamiaoJoints`],
//! [`plant::Pendulums`].
//!
//! keel's payloads are raw bytes; this crate gives two of them a layout: one
//! [`State`] or [`Command`] per joint, back to back, little-endian. Their
//! time is in keel's header, not here: a state's stamp is when its joints
//! were read (or the simulation's time), and a command inherits it.
//!
//! `docs/control.md` describes the whole layer: nodes, cycles, stamps,
//! phases, and how to write a controller.

pub mod can;
pub mod cycle;
pub mod damiao;
pub mod health;
pub mod joints;
pub mod pid;
pub mod plant;

use std::io;
use std::str::FromStr;

/// What a joint reports: radians, and radians per second.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct State {
    /// rad
    pub position: f64,
    /// rad/s
    pub velocity: f64,
    /// The torque the drive gives, N m, as it measures it: what tells a
    /// joint that isn't pushed from one pushed as hard as it can be.
    pub effort: f64,
    /// What's wrong with it, if anything: what its drive says, and what the
    /// joints' loop sees.
    pub status: Status,
}

impl State {
    pub const LEN: usize = 32;

    /// The joints of a `state` payload.
    pub fn read(payload: &[u8]) -> impl Iterator<Item = State> + '_ {
        payload.as_chunks::<{ Self::LEN }>().0.iter().map(|b| State {
            position: f64_at(b, 0),
            velocity: f64_at(b, 8),
            effort: f64_at(b, 16),
            status: Status { flags: u32_at(b, 24), drive: u32_at(b, 28) },
        })
    }

    /// Fills a `state` payload of `states.len() * State::LEN` bytes.
    pub fn write(states: &[State], payload: &mut [u8]) {
        for (state, b) in states.iter().zip(payload.as_chunks_mut::<{ Self::LEN }>().0) {
            b[..8].copy_from_slice(&state.position.to_le_bytes());
            b[8..16].copy_from_slice(&state.velocity.to_le_bytes());
            b[16..24].copy_from_slice(&state.effort.to_le_bytes());
            b[24..28].copy_from_slice(&state.status.flags.to_le_bytes());
            b[28..].copy_from_slice(&state.status.drive.to_le_bytes());
        }
    }
}

/// What's wrong with a joint, if anything. `flags` say what, from the drive
/// (an error, not enabled) and from the joints' loop, which compares what
/// the joint is told with what it does (see [`health`]); `drive` is the
/// drive's own status code, as its maker numbers it (Damiao's: 0 disabled,
/// 1 enabled, 8 and up errors), or 0 when it has none to give.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Status {
    pub flags: u32,
    pub drive: u32,
}

impl Status {
    /// The drive reports an error; its code is in `drive`.
    pub const DRIVE_ERROR: u32 = 1 << 0;
    /// The drive isn't enabled: it holds no torque.
    pub const DISABLED: u32 = 1 << 1;
    /// It gives far less torque than its command asks.
    pub const UNDERPOWERED: u32 = 1 << 2;
    /// It stays far from its target.
    pub const BEHIND: u32 = 1 << 3;
    /// Its reading doesn't change while its target moves.
    pub const FROZEN: u32 = 1 << 4;

    /// Nothing wrong that anyone could see.
    pub fn ok(&self) -> bool {
        self.flags == 0
    }

    pub fn has(&self, flag: u32) -> bool {
        self.flags & flag != 0
    }
}

/// What a joint is asked for, as the drives of quasi-direct-drive motors
/// take it (their MIT mode): the torque
/// `kp (position − θ) + kd (velocity − θ̇) + effort`, in newton metres, where
/// `θ` and `θ̇` are the joint's as it is.
///
/// Whoever drives the joint closes that loop: a drive, many times a cycle,
/// or keel's side of the bus once a cycle for drives that only take a
/// torque. A plain torque is `kp = kd = 0`, which is what [`Command::effort`]
/// makes, and what a controller closing its own loop sends.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Command {
    /// rad
    pub position: f64,
    /// rad/s
    pub velocity: f64,
    /// N m/rad
    pub kp: f64,
    /// N m s/rad
    pub kd: f64,
    /// N m
    pub effort: f64,
}

impl Command {
    pub const LEN: usize = 40;

    /// A plain torque.
    pub fn effort(effort: f64) -> Self {
        Command { effort, ..Default::default() }
    }

    /// The torque it asks for, the joint being at `state`.
    pub fn torque(&self, state: State) -> f64 {
        self.kp * (self.position - state.position) + self.kd * (self.velocity - state.velocity) + self.effort
    }

    pub fn read(payload: &[u8]) -> impl Iterator<Item = Command> + '_ {
        payload.as_chunks::<{ Self::LEN }>().0.iter().map(|b| Command {
            position: f64_at(b, 0),
            velocity: f64_at(b, 8),
            kp: f64_at(b, 16),
            kd: f64_at(b, 24),
            effort: f64_at(b, 32),
        })
    }

    pub fn write(commands: &[Command], payload: &mut [u8]) {
        for (c, b) in commands.iter().zip(payload.as_chunks_mut::<{ Self::LEN }>().0) {
            for (i, x) in [c.position, c.velocity, c.kp, c.kd, c.effort].iter().enumerate() {
                b[i * 8..i * 8 + 8].copy_from_slice(&x.to_le_bytes());
            }
        }
    }
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

fn f64_at(bytes: &[u8], at: usize) -> f64 {
    f64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

/// `--name value` from the command line (a node's `args:`), or `default`.
pub fn arg<T: FromStr>(name: &str, default: T) -> io::Result<T> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg.strip_prefix("--") == Some(name) {
            let value = args.next().unwrap_or_default();
            return value.parse().map_err(|_| io::Error::other(format!("--{name}: can't read {value:?}")));
        }
    }
    Ok(default)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pid::Pid;
    use crate::plant::Pendulum;

    #[test]
    fn payloads_round_trip() {
        let broken = Status { flags: Status::DRIVE_ERROR | Status::BEHIND, drive: 0xD };
        let states = [
            State { position: 1.5, velocity: -0.25, effort: 2.0, status: Status::default() },
            State { position: -3.0, velocity: 8.0, effort: 0.0, status: broken },
        ];
        let mut payload = [0; 2 * State::LEN];
        State::write(&states, &mut payload);
        assert_eq!(State::read(&payload).collect::<Vec<_>>(), states);

        let commands =
            [Command::effort(4.0), Command { position: 1.0, velocity: -2.0, kp: 30.0, kd: 1.5, effort: -0.5 }];
        let mut payload = [0; 2 * Command::LEN];
        Command::write(&commands, &mut payload);
        assert_eq!(Command::read(&payload).collect::<Vec<_>>(), commands);
    }

    #[test]
    fn a_command_asks_for_the_mit_law() {
        let c = Command { position: 1.0, velocity: 0.5, kp: 10.0, kd: 2.0, effort: 0.25 };
        let state = State { position: 0.75, velocity: 1.0, ..Default::default() };
        assert!((c.torque(state) - (10.0 * 0.25 + 2.0 * -0.5 + 0.25)).abs() < 1e-12);
        assert_eq!(Command::effort(3.0).torque(state), 3.0);
    }

    /// The default gains hold the pendulum at an angle, against gravity,
    /// without ever asking for more than the limit.
    #[test]
    fn pid_holds_the_pendulum() {
        let (mut plant, mut pid, dt) = (Pendulum::default(), Pid::default(), 0.001);
        for _ in 0..5000 {
            let effort = pid.update(1.0, plant.state(), dt);
            assert!(effort.abs() <= pid.limit);
            plant.step(effort, dt);
        }
        let state = plant.state();
        assert!((state.position - 1.0).abs() < 0.01 && state.velocity.abs() < 0.01, "{state:?}");
    }
}
