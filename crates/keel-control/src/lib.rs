//! Control on top of keel: joints, a controller, and the buses joints are
//! reached through, all as ordinary nodes.
//!
//! The interface is the dataflow, not a trait: whatever publishes `state` and
//! takes `command` is the hardware. `keel-sim` (a simulated joint) and
//! `keel-can` (joints on a CAN bus) both do, so a controller runs against
//! either unchanged, and swapping them is a change to the dataflow file.
//!
//! keel's payloads are raw bytes; this crate gives two of them a layout: one
//! [`State`] or [`Command`] per joint, back to back, little-endian.

pub mod can;
pub mod cycle;
pub mod pid;
pub mod plant;

use std::io;
use std::str::FromStr;

/// What a joint reports: radians, and radians per second.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct State {
    pub position: f64,
    pub velocity: f64,
}

impl State {
    pub const LEN: usize = 16;

    /// The joints of a `state` payload.
    pub fn read(payload: &[u8]) -> impl Iterator<Item = State> + '_ {
        payload.chunks_exact(Self::LEN).map(|b| State { position: f64_at(b, 0), velocity: f64_at(b, 8) })
    }

    /// Fills a `state` payload of `states.len() * State::LEN` bytes.
    pub fn write(states: &[State], payload: &mut [u8]) {
        for (state, b) in states.iter().zip(payload.chunks_exact_mut(Self::LEN)) {
            b[..8].copy_from_slice(&state.position.to_le_bytes());
            b[8..].copy_from_slice(&state.velocity.to_le_bytes());
        }
    }
}

/// What a joint is asked for: a torque, in newton metres.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Command {
    pub effort: f64,
}

impl Command {
    pub const LEN: usize = 8;

    pub fn read(payload: &[u8]) -> impl Iterator<Item = Command> + '_ {
        payload.chunks_exact(Self::LEN).map(|b| Command { effort: f64_at(b, 0) })
    }

    pub fn write(commands: &[Command], payload: &mut [u8]) {
        for (command, b) in commands.iter().zip(payload.chunks_exact_mut(Self::LEN)) {
            b.copy_from_slice(&command.effort.to_le_bytes());
        }
    }
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
        let states = [State { position: 1.5, velocity: -0.25 }, State { position: -3.0, velocity: 8.0 }];
        let mut payload = [0; 2 * State::LEN];
        State::write(&states, &mut payload);
        assert_eq!(State::read(&payload).collect::<Vec<_>>(), states);

        let commands = [Command { effort: 4.0 }, Command { effort: -0.5 }];
        let mut payload = [0; 2 * Command::LEN];
        Command::write(&commands, &mut payload);
        assert_eq!(Command::read(&payload).collect::<Vec<_>>(), commands);
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
