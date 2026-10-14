//! Control on top of keel: joints, sensors and cameras, the laws that close
//! their loops, and simulations of all of them, as ordinary nodes.
//!
//! In layers, each using only those below it:
//!
//! - **nodes** (`src/bin`): wiring only;
//! - **loops** ([`schedule`]): [`schedule::JointLoop`],
//!   [`schedule::SensorLoop`], [`schedule::CameraLoop`], one per kind of
//!   device, and the scheduler that runs them on the wall clock or on a
//!   simulation's;
//! - **devices** ([`device`]): a trait per kind of device, implemented by
//!   real drivers ([`can`]) and by simulated devices ([`sim`]), which wear a
//!   hardware model ([`drive`], [`model`]) over a physics engine;
//! - **laws** ([`law`]): pure maths, such as a PID;
//! - **messages** (here): the layouts of the payloads.
//!
//! On a robot, every device is a node of its own: its loop over its driver.
//! The dataflow is the interface: whatever publishes `state` and takes
//! `command` is the joints. In a simulation, the same loops run in one node,
//! over simulated devices that share a world.
//!
//! keel's payloads are raw bytes; this crate gives four of them a layout,
//! little-endian: [`State`] and [`Command`] per joint, back to back; a
//! [`vector`] of numbers; an [`Image`]. Their time is in keel's header: a
//! state's stamp is when its joints were read (or the simulation's time),
//! and a command inherits it.
//!
//! `docs/control.md` describes the whole layer.

pub mod can;
pub mod cycle;
pub mod device;
pub mod drive;
pub mod law;
pub mod model;
pub mod plant;
pub mod schedule;
pub mod sim;

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
        payload.as_chunks::<{ Self::LEN }>().0.iter().map(|b| State { position: f64_at(b, 0), velocity: f64_at(b, 8) })
    }

    /// Fills a `state` payload of `states.len() * State::LEN` bytes.
    pub fn write(states: &[State], payload: &mut [u8]) {
        for (state, b) in states.iter().zip(payload.as_chunks_mut::<{ Self::LEN }>().0) {
            b[..8].copy_from_slice(&state.position.to_le_bytes());
            b[8..].copy_from_slice(&state.velocity.to_le_bytes());
        }
    }
}

/// What a command asks of a joint: where to be, how fast to go, or how hard
/// to push. Whoever closes the loop for the mode, the drive or keel (see
/// [`schedule::LoopAt`]), turns it into a torque.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
#[repr(u8)]
pub enum Mode {
    /// A torque, in newton metres.
    #[default]
    Effort = 0,
    /// A position, in radians.
    Position = 1,
    /// A velocity, in radians per second.
    Velocity = 2,
}

impl Mode {
    pub(crate) fn from_u8(b: u8) -> Option<Mode> {
        [Mode::Effort, Mode::Position, Mode::Velocity].get(b as usize).copied()
    }
}

impl FromStr for Mode {
    type Err = io::Error;

    fn from_str(s: &str) -> io::Result<Mode> {
        match s {
            "effort" => Ok(Mode::Effort),
            "position" => Ok(Mode::Position),
            "velocity" => Ok(Mode::Velocity),
            other => Err(io::Error::other(format!("`effort`, `position` or `velocity`, not {other:?}"))),
        }
    }
}

/// What a joint is asked for: a value, in the terms of its mode.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Command {
    pub mode: Mode,
    pub value: f64,
}

impl Command {
    /// The value, then the mode in a byte, then padding.
    pub const LEN: usize = 16;

    pub fn effort(value: f64) -> Command {
        Command { mode: Mode::Effort, value }
    }

    pub fn position(value: f64) -> Command {
        Command { mode: Mode::Position, value }
    }

    /// The joints' commands in a payload; one with a mode this version
    /// doesn't know is a zero effort.
    pub fn read(payload: &[u8]) -> impl Iterator<Item = Command> + '_ {
        payload.as_chunks::<{ Self::LEN }>().0.iter().map(|b| match Mode::from_u8(b[8]) {
            Some(mode) => Command { mode, value: f64_at(b, 0) },
            None => Command::default(),
        })
    }

    pub fn write(commands: &[Command], payload: &mut [u8]) {
        for (command, b) in commands.iter().zip(payload.as_chunks_mut::<{ Self::LEN }>().0) {
            b[..8].copy_from_slice(&command.value.to_le_bytes());
            b[8..].fill(0);
            b[8] = command.mode as u8;
        }
    }
}

/// A sensor's reading: numbers, back to back as `f64`s. What they are (an
/// IMU's acceleration then rate of turn, a wrist's force then torque) is the
/// sensor's to say, in its settings, not the message's.
pub mod vector {
    pub fn read(payload: &[u8]) -> impl Iterator<Item = f64> + '_ {
        payload.as_chunks::<8>().0.iter().map(|b| super::f64_at(b, 0))
    }

    pub fn write(values: &[f64], payload: &mut [u8]) {
        for (value, b) in values.iter().zip(payload.as_chunks_mut::<8>().0) {
            b.copy_from_slice(&value.to_le_bytes());
        }
    }
}

/// How an image's pixels are laid out.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(u32)]
pub enum Format {
    /// Three bytes a pixel, red, green, blue; rows from the top.
    #[default]
    Rgb8 = 0,
    /// An `f32` a pixel: the distance in metres, 0 where nothing came back.
    DepthF32 = 1,
}

impl Format {
    pub fn bytes_per_pixel(self) -> usize {
        match self {
            Format::Rgb8 => 3,
            Format::DepthF32 => 4,
        }
    }
}

/// A camera's picture: a 16-byte header (width, height, format, and a
/// spare, as `u32`s), then the pixels.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub format: Format,
    pub pixels: Vec<u8>,
}

impl Image {
    pub const HEADER_LEN: usize = 16;

    pub fn new(width: u32, height: u32, format: Format) -> Image {
        Image { width, height, format, pixels: vec![0; width as usize * height as usize * format.bytes_per_pixel()] }
    }

    pub fn len(&self) -> usize {
        Self::HEADER_LEN + self.pixels.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pixels.is_empty()
    }

    pub fn write(&self, payload: &mut [u8]) {
        payload[0..4].copy_from_slice(&self.width.to_le_bytes());
        payload[4..8].copy_from_slice(&self.height.to_le_bytes());
        payload[8..12].copy_from_slice(&(self.format as u32).to_le_bytes());
        payload[12..16].fill(0);
        payload[Self::HEADER_LEN..].copy_from_slice(&self.pixels);
    }

    /// The header of an image payload, and its pixels, read in place.
    pub fn read(payload: &[u8]) -> Option<(u32, u32, Format, &[u8])> {
        let u32_at = |at: usize| u32::from_le_bytes(payload[at..at + 4].try_into().unwrap());
        if payload.len() < Self::HEADER_LEN {
            return None;
        }
        let (width, height) = (u32_at(0), u32_at(4));
        let format = match u32_at(8) {
            0 => Format::Rgb8,
            1 => Format::DepthF32,
            _ => return None,
        };
        let pixels = &payload[Self::HEADER_LEN..];
        (pixels.len() == width as usize * height as usize * format.bytes_per_pixel())
            .then_some((width, height, format, pixels))
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
    use crate::law::Pid;
    use crate::plant::Pendulum;

    #[test]
    fn payloads_round_trip() {
        let states = [State { position: 1.5, velocity: -0.25 }, State { position: -3.0, velocity: 8.0 }];
        let mut payload = [0; 2 * State::LEN];
        State::write(&states, &mut payload);
        assert_eq!(State::read(&payload).collect::<Vec<_>>(), states);

        let commands = [Command::effort(4.0), Command::position(-0.5), Command { mode: Mode::Velocity, value: 2.0 }];
        let mut payload = [0; 3 * Command::LEN];
        Command::write(&commands, &mut payload);
        assert_eq!(Command::read(&payload).collect::<Vec<_>>(), commands);

        let values = [1.0, -2.5, 9.81];
        let mut payload = [0; 24];
        vector::write(&values, &mut payload);
        assert_eq!(vector::read(&payload).collect::<Vec<_>>(), values);

        let mut image = Image::new(2, 1, Format::Rgb8);
        image.pixels.copy_from_slice(&[1, 2, 3, 4, 5, 6]);
        let mut payload = vec![0; image.len()];
        image.write(&mut payload);
        assert_eq!(Image::read(&payload), Some((2, 1, Format::Rgb8, &image.pixels[..])));
        assert_eq!(Image::read(&payload[..payload.len() - 1]), None, "short");
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
