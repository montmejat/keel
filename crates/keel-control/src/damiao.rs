//! Damiao's motors in MIT mode, from the bus master's side: the frames
//! `keel-can --protocol damiao` sends and reads. From Damiao's manual for
//! the DM-J4310-2EC and their reference driver (`DM_CAN.py`), and the same
//! as OpenArm's `openarm_can`.
//!
//! - **A command** goes to the motor's CAN id: position, velocity, two gains
//!   and a torque, in 16, 12, 12, 12 and 12 bits: a [`Command`] as it is.
//! - **Special frames** go to the same id: seven `FF` bytes, then `FC` to
//!   enable, `FD` to disable, `FE` to make the present position zero, `FB`
//!   to clear an error.
//! - **The motor answers each frame** with feedback on its master id:
//!   status, position, velocity, torque, temperatures.
//! - **Registers** are read and written through id `0x7FF`, and answered on
//!   the master id. Among them, the ranges positions, velocities and
//!   torques are packed into (PMAX, VMAX, TMAX): set per motor, so read from
//!   it rather than assumed.
//!
//! Classic CAN or CAN FD: the frames are 8 bytes either way.

use crate::{Command, State};

/// Where register requests go, for every motor on the bus.
pub const REGISTER_ID: u32 = 0x7FF;

pub const KP_MAX: f64 = 500.0;
pub const KD_MAX: f64 = 5.0;

/// Registers, by number.
pub mod reg {
    pub const MST_ID: u8 = 7;
    pub const CTRL_MODE: u8 = 10;
    pub const PMAX: u8 = 21;
    pub const VMAX: u8 = 22;
    pub const TMAX: u8 = 23;

    /// CTRL_MODE's value for MIT mode.
    pub const MIT_MODE: u32 = 1;
}

/// The status in a feedback frame's high nibble, in words.
pub fn status_name(status: u8) -> &'static str {
    match status {
        0x0 => "disabled",
        0x1 => "enabled",
        0x8 => "overvoltage",
        0x9 => "undervoltage",
        0xA => "overcurrent",
        0xB => "drive overheated",
        0xC => "coil overheated",
        0xD => "communication lost",
        0xE => "overload",
        _ => "unknown status",
    }
}

/// Statuses from here up are errors: the motor holds no torque until one is
/// cleared.
pub const ERRORS: u8 = 0x8;

/// The ranges a motor packs position (rad), velocity (rad/s) and torque
/// (N m) into.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Limits {
    pub p: f64,
    pub v: f64,
    pub t: f64,
}

fn to_uint(x: f64, min: f64, max: f64, bits: u32) -> u16 {
    ((x.clamp(min, max) - min) / (max - min) * ((1u32 << bits) - 1) as f64) as u16
}

fn to_float(x: u16, min: f64, max: f64, bits: u32) -> f64 {
    x as f64 / ((1u32 << bits) - 1) as f64 * (max - min) + min
}

/// A command, packed. Values beyond the motor's ranges are clamped to them.
pub fn command(c: &Command, l: Limits) -> [u8; 8] {
    let p = to_uint(c.position, -l.p, l.p, 16);
    let v = to_uint(c.velocity, -l.v, l.v, 12);
    let kp = to_uint(c.kp, 0.0, KP_MAX, 12);
    let kd = to_uint(c.kd, 0.0, KD_MAX, 12);
    let t = to_uint(c.effort, -l.t, l.t, 12);
    [
        (p >> 8) as u8,
        p as u8,
        (v >> 4) as u8,
        ((v & 0xF) << 4) as u8 | (kp >> 8) as u8,
        kp as u8,
        (kd >> 4) as u8,
        ((kd & 0xF) << 4) as u8 | (t >> 8) as u8,
        t as u8,
    ]
}

pub const ENABLE: u8 = 0xFC;
pub const DISABLE: u8 = 0xFD;
pub const ZERO: u8 = 0xFE;
pub const CLEAR_ERROR: u8 = 0xFB;

/// One of the special frames: [`ENABLE`], [`DISABLE`], [`ZERO`],
/// [`CLEAR_ERROR`].
pub fn special(which: u8) -> [u8; 8] {
    let mut d = [0xFF; 8];
    d[7] = which;
    d
}

/// What a motor says after each frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Feedback {
    pub status: u8,
    pub state: State,
    /// N m
    pub torque: f64,
}

/// A feedback frame, if that's what it is: feedback and register answers
/// share the master id, and an answer's third byte is `0x33` or `0x55`, a
/// position's low byte may be too. Ask for registers when no commands are
/// in flight.
pub fn feedback(d: &[u8; 8], l: Limits) -> Feedback {
    let p = (d[1] as u16) << 8 | d[2] as u16;
    let v = (d[3] as u16) << 4 | (d[4] >> 4) as u16;
    let t = ((d[4] & 0xF) as u16) << 8 | d[5] as u16;
    Feedback {
        status: d[0] >> 4,
        state: State { position: to_float(p, -l.p, l.p, 16), velocity: to_float(v, -l.v, l.v, 12) },
        torque: to_float(t, -l.t, l.t, 12),
    }
}

const READ: u8 = 0x33;
const WRITE: u8 = 0x55;

/// A register read, for the motor whose CAN id is `motor`.
pub fn read_register(motor: u32, rid: u8) -> [u8; 8] {
    [motor as u8, (motor >> 8) as u8, READ, rid, 0, 0, 0, 0]
}

/// A register write: `value` is a `u32`'s or an `f32`'s little-endian bytes.
pub fn write_register(motor: u32, rid: u8, value: [u8; 4]) -> [u8; 8] {
    let [a, b, c, d] = value;
    [motor as u8, (motor >> 8) as u8, WRITE, rid, a, b, c, d]
}

/// Asks a motor for feedback, without commanding anything.
pub fn refresh(motor: u32) -> [u8; 8] {
    [motor as u8, (motor >> 8) as u8, 0xCC, 0, 0, 0, 0, 0]
}

/// A register's value in an answer: the motor's id, the register, the value's
/// bytes.
pub fn register_answer(d: &[u8; 8]) -> Option<(u32, u8, [u8; 4])> {
    matches!(d[2], READ | WRITE).then(|| (u16::from_le_bytes([d[0], d[1]]) as u32, d[3], [d[4], d[5], d[6], d[7]]))
}

#[cfg(test)]
mod tests {
    use super::*;

    const L: Limits = Limits { p: 12.5, v: 30.0, t: 10.0 };

    #[test]
    fn a_zero_command_packs_as_the_reference_driver_packs_it() {
        assert_eq!(command(&Command::default(), L), [0x7F, 0xFF, 0x7F, 0xF0, 0x00, 0x00, 0x07, 0xFF]);
    }

    #[test]
    fn a_command_packs_its_fields_where_they_go() {
        let c = Command { position: 12.5, velocity: -30.0, kp: 500.0, kd: 0.0, effort: 10.0 };
        // p all ones; v all zeros; kp all ones; kd zeros; t all ones.
        assert_eq!(command(&c, L), [0xFF, 0xFF, 0x00, 0x0F, 0xFF, 0x00, 0x0F, 0xFF]);
    }

    #[test]
    fn feedback_unpacks() {
        // Enabled, motor 2, mid-range position, velocity and torque.
        let f = feedback(&[0x12, 0x7F, 0xFF, 0x7F, 0xF7, 0xFF, 25, 26], L);
        assert_eq!(f.status, 1);
        assert!(f.state.position.abs() < 1e-3 && f.state.velocity.abs() < 0.01 && f.torque.abs() < 0.01, "{f:?}");
    }

    #[test]
    fn registers() {
        assert_eq!(special(ENABLE), [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFC]);
        assert_eq!(read_register(0x102, reg::PMAX), [0x02, 0x01, 0x33, 21, 0, 0, 0, 0]);
        let w = write_register(0x01, reg::CTRL_MODE, 1u32.to_le_bytes());
        assert_eq!(register_answer(&w), Some((0x01, reg::CTRL_MODE, [1, 0, 0, 0])));
        assert_eq!(register_answer(&[0x11, 0x7F, 0xFF, 0, 0, 0, 0, 0]), None);
    }
}
