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
//!
//! [`DamiaoJoints`] is a bus of them as joints, for `keel-can`.

use std::io;
use std::time::{Duration, Instant};

use keel::trace::now_ns;

use crate::can::{Bus, Frame};
use crate::joints::Joints;
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

/// A Damiao status as a joint's: its code, and whether it's an error or a
/// disabled motor.
fn status_of(code: u8) -> crate::Status {
    use crate::Status;
    let flags = match code {
        0 => Status::DISABLED,
        c if c >= ERRORS => Status::DRIVE_ERROR,
        _ => 0,
    };
    Status { flags, drive: code as u32 }
}

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

/// What a motor says after each frame: its status, and its state, the torque
/// it gives included.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Feedback {
    pub status: u8,
    pub state: State,
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
        state: State {
            position: to_float(p, -l.p, l.p, 16),
            velocity: to_float(v, -l.v, l.v, 12),
            effort: to_float(t, -l.t, l.t, 12),
            status: status_of(d[0] >> 4),
        },
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

/// How long a motor has to answer a register request, before asking again.
const REGISTER_TIMEOUT: Duration = Duration::from_millis(100);

/// Damiao's motors on a bus, as joints, in MIT mode: the command as it is,
/// the loop closing on the motors.
///
/// At start, each is put in MIT mode, its ranges are read from its
/// registers, and it's enabled; at stop, it's disabled. A motor that doesn't
/// answer is waited for, and said so: the robot (or drydock) may be started
/// after keel. The motors speak
/// only when spoken to: each cycle's round of frames brings their answers,
/// which are the next state, stamped with when the round went out. Before
/// the first command, the round asks for feedback without commanding
/// anything.
pub struct DamiaoJoints {
    bus: Bus,
    interface: String,
    fd: bool,
    motors: Vec<Motor>,
    fresh: Vec<bool>,
    /// When the last round went out.
    round_ns: u64,
}

struct Motor {
    id: u32,
    /// The id it answers on.
    master: u32,
    limits: Limits,
    status: Option<u8>,
}

impl DamiaoJoints {
    /// `motors`: each motor's CAN id and the id it answers on, in joint
    /// order; `fd`: whether they're on CAN FD.
    pub fn open(interface: &str, motors: &[(u32, u32)], fd: bool) -> io::Result<Self> {
        if motors.is_empty() {
            return Err(io::Error::other("no motors"));
        }
        let bus = if fd { Bus::open_fd(interface)? } else { Bus::open(interface)? };
        let motors: Vec<Motor> = motors
            .iter()
            .map(|&(id, master)| Motor { id, master, limits: Limits { p: 1.0, v: 1.0, t: 1.0 }, status: None })
            .collect();
        let fresh = vec![false; motors.len()];
        Ok(Self { bus, interface: interface.into(), fd, motors, fresh, round_ns: now_ns() })
    }

    /// `0x01:0x11,0x02:0x12`: each motor's id and the id it answers on.
    pub fn parse_motors(motors: &str) -> io::Result<Vec<(u32, u32)>> {
        let id = |s: &str| {
            let s = s.trim();
            match s.strip_prefix("0x") {
                Some(hex) => u32::from_str_radix(hex, 16),
                None => s.parse(),
            }
            .map_err(|_| io::Error::other(format!("--motors: {s:?} isn't an id")))
        };
        motors
            .split(',')
            .filter(|m| !m.trim().is_empty())
            .map(|m| {
                let (a, b) =
                    m.split_once(':').ok_or_else(|| io::Error::other(format!("--motors: {m:?} isn't id:master_id")))?;
                Ok((id(a)?, id(b)?))
            })
            .collect()
    }

    fn frame(&self, id: u32, data: [u8; 8]) -> Frame {
        Frame::new(id, &data).fd(self.fd)
    }

    /// Sends a register request for motor `i` until it answers, saying once
    /// that it's waiting.
    fn ask(&self, i: usize, request: [u8; 8]) -> io::Result<[u8; 4]> {
        let mut waited = false;
        loop {
            if let Some(value) = self.ask_once(i, request)? {
                if waited {
                    println!("motor {:#04x} answers", self.motors[i].id);
                }
                return Ok(value);
            }
            if !waited {
                println!("waiting for {}: is the robot, or drydock, running?", self.describe(i));
                waited = true;
            }
        }
    }

    /// Motor `i`, and where its answers should come from.
    fn describe(&self, i: usize) -> String {
        let m = &self.motors[i];
        let fd = if self.fd { ", in CAN FD" } else { "" };
        format!("motor {:#04x} on `{}` (answering on {:#04x}{fd})", m.id, self.interface, m.master)
    }

    /// Sends a register request for motor `i`, and waits a little for its
    /// answer.
    fn ask_once(&self, i: usize, request: [u8; 8]) -> io::Result<Option<[u8; 4]>> {
        let m = &self.motors[i];
        self.bus.send(&self.frame(REGISTER_ID, request))?;
        let until = Instant::now() + REGISTER_TIMEOUT;
        while let Some(left) = until.checked_duration_since(Instant::now()) {
            if !self.bus.wait(left)? {
                continue;
            }
            while let Some(frame) = self.bus.try_recv()? {
                let answer = frame.eight().filter(|_| frame.id == m.master).and_then(|d| register_answer(&d));
                if let Some((id, rid, value)) = answer {
                    if id == m.id && rid == request[3] {
                        return Ok(Some(value));
                    }
                }
            }
        }
        Ok(None)
    }
}

impl Joints for DamiaoJoints {
    fn count(&self) -> usize {
        self.motors.len()
    }

    /// Whether each motor answers: a warning, not a failure, if one doesn't,
    /// since the robot (or drydock) may just not be started yet.
    fn check(&mut self) -> io::Result<()> {
        use keel::check::{report, Level};
        let what = format!("motors on {}", self.interface);
        let silent: Vec<usize> = (0..self.motors.len())
            .filter_map(|i| match self.ask_once(i, read_register(self.motors[i].id, reg::MST_ID)) {
                Ok(Some(_)) => None,
                Ok(None) => Some(Ok(i)),
                Err(e) => Some(Err(e)),
            })
            .collect::<io::Result<_>>()?;
        let n = self.motors.len();
        match silent.len() {
            0 => report(Level::Ok, &what, &format!("all {n} answer")),
            k if k == n => report(
                Level::Warn,
                &what,
                &format!("none of the {n} answers: start the robot, or drydock, before running"),
            ),
            _ => {
                let list: Vec<String> = silent.iter().map(|&i| self.describe(i)).collect();
                report(Level::Warn, &what, &format!("no answer from {}", list.join("; ")));
            }
        }
        Ok(())
    }

    /// Each motor in MIT mode, its ranges read, then enabled: one request at
    /// a time, nothing else on the bus.
    fn start(&mut self) -> io::Result<()> {
        for i in 0..self.motors.len() {
            let id = self.motors[i].id;
            let mode =
                u32::from_le_bytes(self.ask(i, write_register(id, reg::CTRL_MODE, reg::MIT_MODE.to_le_bytes()))?);
            if mode != reg::MIT_MODE {
                return Err(io::Error::other(format!(
                    "motor {id:#04x} stays in mode {mode}: keel speaks MIT mode ({})",
                    reg::MIT_MODE
                )));
            }
            let range =
                |rid| -> io::Result<f64> { Ok(f32::from_le_bytes(self.ask(i, read_register(id, rid))?) as f64) };
            let limits = Limits { p: range(reg::PMAX)?, v: range(reg::VMAX)?, t: range(reg::TMAX)? };
            self.motors[i].limits = limits;
            println!(
                "motor {id:#04x}: MIT mode, ±{} rad, ±{} rad/s, ±{} N m, answering on {:#04x}",
                limits.p, limits.v, limits.t, self.motors[i].master
            );
        }
        for m in &self.motors {
            self.bus.send(&self.frame(m.id, special(ENABLE)))?;
        }
        Ok(())
    }

    fn read(&mut self, states: &mut [State]) -> io::Result<Option<u64>> {
        while let Some(frame) = self.bus.try_recv()? {
            let Some(i) = self.motors.iter().position(|m| m.master == frame.id) else { continue };
            let Some(data) = frame.eight() else { continue };
            let m = &mut self.motors[i];
            let f = feedback(&data, m.limits);
            if m.status != Some(f.status) {
                if f.status >= ERRORS || m.status.is_some() {
                    println!("motor {:#04x}: {} ({:X})", m.id, status_name(f.status), f.status);
                }
                m.status = Some(f.status);
            }
            (states[i], self.fresh[i]) = (f.state, true);
        }
        if !self.fresh.iter().all(|&f| f) {
            return Ok(None);
        }
        self.fresh.fill(false);
        Ok(Some(self.round_ns))
    }

    fn write(&mut self, commands: Option<&[Command]>, _states: &[State]) -> io::Result<()> {
        self.round_ns = now_ns();
        for (i, m) in self.motors.iter().enumerate() {
            let frame = match commands {
                Some(c) => self.frame(m.id, command(&c[i], m.limits)),
                None => self.frame(REGISTER_ID, refresh(m.id)),
            };
            self.bus.send(&frame)?;
        }
        Ok(())
    }

    fn stop(&mut self) -> io::Result<()> {
        for m in &self.motors {
            self.bus.send(&self.frame(m.id, special(DISABLE)))?;
        }
        Ok(())
    }
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
        assert!(f.state.position.abs() < 1e-3 && f.state.velocity.abs() < 0.01 && f.state.effort.abs() < 0.01, "{f:?}");
    }

    #[test]
    fn motors_parse() {
        assert_eq!(DamiaoJoints::parse_motors("0x01:0x11, 2:18").unwrap(), [(1, 0x11), (2, 18)]);
        assert!(DamiaoJoints::parse_motors("0x01").is_err());
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
