//! Dynamixel servos (Protocol 2.0) on a serial bus, from the bus master's
//! side: the packets `keel-dynamixel` sends and reads, from ROBOTIS's
//! Protocol 2.0 e-Manual, and the XL330's control table. The same as
//! Pollen's runtime does on the Microduck (`duck-control`'s `bus.rs`):
//!
//! - **Each cycle, one Fast Sync Read** of twelve bytes at 124 (PWM,
//!   current, velocity, position) from every servo, and the IMU board's
//!   block from it if there's one, all answered in one packet; then **one
//!   Sync Write** of the goal positions.
//! - **A command's position is its servo's goal** ([0034]): the servo closes
//!   its own loop, with the P gain set at start.
//!
//! [0034]: ../../../docs/decisions/0034-dynamixel-servos.md
//!
//! [`DynamixelJoints`] is a bus of them as joints, for `keel-dynamixel`.

use std::f64::consts::TAU;
use std::io;
use std::time::{Duration, Instant};

use keel::trace::now_ns;

use crate::joints::Joints;
use crate::serial::Port;
use crate::{Command, Imu, State, Status};

pub const BROADCAST: u8 = 0xFE;
const HEADER: [u8; 4] = [0xFF, 0xFF, 0xFD, 0x00];

/// Instructions, by number.
pub mod instruction {
    pub const PING: u8 = 0x01;
    pub const WRITE: u8 = 0x03;
    pub const SYNC_WRITE: u8 = 0x83;
    pub const FAST_SYNC_READ: u8 = 0x8A;
    pub const STATUS: u8 = 0x55;
}

/// The XL330's control table, what keel uses of it.
pub mod addr {
    pub const TORQUE_ENABLE: u16 = 64;
    pub const POSITION_P_GAIN: u16 = 84;
    pub const GOAL_POSITION: u16 = 116;
    /// Where the block read each cycle starts: present PWM (2 bytes),
    /// current (2), velocity (4), position (4).
    pub const BLOCK: u16 = 124;
    pub const BLOCK_LEN: u16 = 12;
}

/// The status error byte's alert bit: the servo has a hardware error.
pub const ALERT: u8 = 0x80;

/// Radians per position count, the horn's centre at 2048 (as Pollen's
/// runtime: `rad = 2π·raw/4096 − π`).
const RAD_PER_COUNT: f64 = TAU / 4096.0;
const CENTRE: f64 = 2048.0;
/// Velocity counts are 0.229 rev/min.
const RAD_S_PER_COUNT: f64 = 0.229 * TAU / 60.0;
/// The XL330's torque constant, N m/A: BAM's measured model of it
/// (Rhoban, `bam/params/xl330/m6.json`). Its current is the supply's, which
/// makes the effort an estimate.
const TORQUE_PER_AMP: f64 = 0.366;

/// The CRC-16 of Protocol 2.0: polynomial 0x8005, from 0, most significant
/// bit first.
pub fn crc(bytes: &[u8]) -> u16 {
    let mut crc = 0u16;
    for &b in bytes {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x8005 } else { crc << 1 };
        }
    }
    crc
}

/// An instruction packet: stuffed, with its length and CRC.
pub fn packet(id: u8, instruction: u8, params: &[u8]) -> Vec<u8> {
    let mut body = vec![instruction];
    body.extend_from_slice(params);
    // FF FF FD inside the packet gets an extra FD, so it isn't a header.
    let mut stuffed = Vec::with_capacity(body.len() + 4);
    for &b in &body {
        stuffed.push(b);
        if stuffed.len() >= 3 && stuffed[stuffed.len() - 3..] == [0xFF, 0xFF, 0xFD] {
            stuffed.push(0xFD);
        }
    }
    let mut out = HEADER.to_vec();
    out.push(id);
    out.extend(((stuffed.len() + 2) as u16).to_le_bytes());
    out.extend(stuffed);
    out.extend(crc(&out).to_le_bytes());
    out
}

/// A Fast Sync Read of `len` bytes at `address` from `ids`, in that order.
pub fn fast_sync_read(address: u16, len: u16, ids: &[u8]) -> Vec<u8> {
    let mut params = address.to_le_bytes().to_vec();
    params.extend(len.to_le_bytes());
    params.extend_from_slice(ids);
    packet(BROADCAST, instruction::FAST_SYNC_READ, &params)
}

/// A Sync Write of each id's `data`, all as long, at `address`.
pub fn sync_write(address: u16, writes: &[(u8, Vec<u8>)]) -> Vec<u8> {
    let len = writes.first().map_or(0, |(_, d)| d.len()) as u16;
    let mut params = address.to_le_bytes().to_vec();
    params.extend(len.to_le_bytes());
    for (id, data) in writes {
        params.push(*id);
        params.extend_from_slice(data);
    }
    packet(BROADCAST, instruction::SYNC_WRITE, &params)
}

/// One device's part of a Fast Sync Read's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    pub id: u8,
    pub error: u8,
    pub data: Vec<u8>,
}

/// The answer to a Fast Sync Read of `len` bytes from `n` devices, at the
/// start of `bytes` after any noise: one packet, `[error, id, data, CRC]`
/// per device, each CRC over the packet up to it (the spec's Fast Sync
/// Read). `Ok(None)` until it's all there.
pub fn fast_reply(bytes: &[u8], n: usize, len: usize) -> io::Result<Option<Vec<Part>>> {
    let Some(start) = bytes.windows(4).position(|w| w == HEADER) else { return Ok(None) };
    let p = &bytes[start..];
    let size = 8 + n * (4 + len);
    if p.len() < size {
        return Ok(None);
    }
    if p[4] != BROADCAST || p[7] != instruction::STATUS {
        return Err(io::Error::other("the answer isn't a Fast Sync Read's"));
    }
    let mut parts = Vec::with_capacity(n);
    let mut at = 8;
    for _ in 0..n {
        let end = at + 2 + len;
        let want = crc(&p[..end]);
        let got = u16::from_le_bytes([p[end], p[end + 1]]);
        if want != got {
            return Err(io::Error::other(format!(
                "a Fast Sync Read answer with a bad CRC ({got:#06x}, not {want:#06x})"
            )));
        }
        parts.push(Part { error: p[at], id: p[at + 1], data: p[at + 2..end].to_vec() });
        at = end + 2;
    }
    Ok(Some(parts))
}

/// A status packet (to a ping, a read, a write) at the start of `bytes`
/// after any noise: its id, error byte and parameters. `None` until whole.
pub fn status(bytes: &[u8]) -> Option<(u8, u8, Vec<u8>)> {
    let start = bytes.windows(4).position(|w| w == HEADER)?;
    let p = &bytes[start..];
    let len = u16::from_le_bytes([*p.get(5)?, *p.get(6)?]) as usize;
    let end = 7 + len;
    if p.len() < end || len < 4 || p[7] != instruction::STATUS {
        return None;
    }
    let got = u16::from_le_bytes([p[end - 2], p[end - 1]]);
    if crc(&p[..end - 2]) != got {
        return None;
    }
    // Unstuffing: an FD after FF FF FD is the stuffing's.
    let mut params: Vec<u8> = Vec::new();
    for &b in &p[9..end - 2] {
        if b == 0xFD && params.len() >= 3 && params[params.len() - 3..] == [0xFF, 0xFF, 0xFD] {
            continue;
        }
        params.push(b);
    }
    Some((p[4], p[8], params))
}

/// A joint's state from its servo's block.
pub fn state(part: &Part) -> State {
    let d = &part.data;
    let current = i16::from_le_bytes([d[2], d[3]]) as f64 / 1e3;
    let velocity = i32::from_le_bytes([d[4], d[5], d[6], d[7]]) as f64;
    let position = i32::from_le_bytes([d[8], d[9], d[10], d[11]]) as f64;
    let status = if part.error & ALERT != 0 {
        Status { flags: Status::DRIVE_ERROR, drive: part.error as u32 }
    } else {
        Status::default()
    };
    State {
        position: (position - CENTRE) * RAD_PER_COUNT,
        velocity: velocity * RAD_S_PER_COUNT,
        effort: current * TORQUE_PER_AMP,
        status,
    }
}

/// A goal position, in counts, for `rad`: within the 0 to 4095 a turn has.
pub fn goal(rad: f64) -> i32 {
    (CENTRE + rad / RAD_PER_COUNT).round().clamp(0.0, 4095.0) as i32
}

/// The Microduck's IMU board's block (Pollen's `imu.rs`): the gyro, x y z,
/// `i16` at 17.5 mdps a count, then the chip's fused orientation, x y z of a
/// unit quaternion as half-floats, its w ≥ 0 derived. Turned into the body's
/// frame by the board's `mount` (w x y z): `body = mount · board`. Its
/// acceleration isn't in the block: NaN.
pub fn imu(block: &[u8], mount: [f64; 4]) -> Imu {
    let gyro = [0, 2, 4].map(|i| i16::from_le_bytes([block[i], block[i + 1]]) as f64 * 0.0175 * TAU / 360.0);
    let [x, y, z] = [6, 8, 10].map(|i| half(u16::from_le_bytes([block[i], block[i + 1]])));
    let w = (1.0 - x * x - y * y - z * z).max(0.0).sqrt();
    Imu {
        orientation: multiply([w, x, y, z], conjugate(mount)),
        angular_velocity: rotate(mount, gyro),
        linear_acceleration: [f64::NAN; 3],
    }
}

fn half(h: u16) -> f64 {
    let sign = if h & 0x8000 != 0 { -1.0 } else { 1.0 };
    let (e, m) = ((h >> 10) & 0x1F, (h & 0x3FF) as f64);
    sign * if e == 0 { m * 2f64.powi(-24) } else { (1.0 + m / 1024.0) * 2f64.powi(e as i32 - 15) }
}

fn conjugate(q: [f64; 4]) -> [f64; 4] {
    [q[0], -q[1], -q[2], -q[3]]
}

fn multiply(a: [f64; 4], b: [f64; 4]) -> [f64; 4] {
    [
        a[0] * b[0] - a[1] * b[1] - a[2] * b[2] - a[3] * b[3],
        a[0] * b[1] + a[1] * b[0] + a[2] * b[3] - a[3] * b[2],
        a[0] * b[2] - a[1] * b[3] + a[2] * b[0] + a[3] * b[1],
        a[0] * b[3] + a[1] * b[2] - a[2] * b[1] + a[3] * b[0],
    ]
}

fn rotate(q: [f64; 4], v: [f64; 3]) -> [f64; 3] {
    let p = multiply(multiply(q, [0.0, v[0], v[1], v[2]]), conjugate(q));
    [p[1], p[2], p[3]]
}

/// How long a servo has to answer.
const ANSWER: Duration = Duration::from_millis(20);

/// Dynamixel servos on a serial bus as joints: one per id in `ids`, in
/// joint order, and an IMU board on the same bus if there's one.
pub struct DynamixelJoints {
    port: Port,
    ids: Vec<u8>,
    /// The IMU board's id and its mount, w x y z.
    imu: Option<(u8, [f64; 4])>,
    p_gain: u16,
    /// The latest IMU reading, sent with the state it came with.
    reading: Option<Imu>,
    /// Whether a command's unused fields have been mentioned.
    mentioned: bool,
    buf: Vec<u8>,
}

impl DynamixelJoints {
    pub fn open(path: &str, baud: u32, ids: Vec<u8>, imu: Option<(u8, [f64; 4])>, p_gain: u16) -> io::Result<Self> {
        if ids.is_empty() {
            return Err(io::Error::other("--motors: no servos"));
        }
        let port = Port::open(path, baud)?;
        Ok(Self { port, ids, imu, p_gain, reading: None, mentioned: false, buf: Vec::new() })
    }

    /// `20,21,22`: the servos' ids, in joint order.
    pub fn parse_ids(list: &str) -> io::Result<Vec<u8>> {
        list.split(',')
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.trim().parse().map_err(|_| io::Error::other(format!("--motors: {s:?} isn't a servo id"))))
            .collect()
    }

    /// `w,x,y,z`: the IMU board's mount.
    pub fn parse_mount(text: &str) -> io::Result<[f64; 4]> {
        let q: Vec<f64> = text
            .split(',')
            .map(|s| s.trim().parse::<f64>())
            .collect::<Result<_, _>>()
            .map_err(|_| io::Error::other(format!("--imu-mount {text:?}: a quaternion, w,x,y,z")))?;
        let q: [f64; 4] = q.try_into().map_err(|_| io::Error::other("--imu-mount: four numbers, w,x,y,z"))?;
        let n = q.iter().map(|x| x * x).sum::<f64>().sqrt();
        Ok(q.map(|x| x / n))
    }

    /// Sends `packet`, and waits for a status packet from `id`.
    fn ask(&mut self, packet: &[u8], id: u8) -> io::Result<Option<(u8, Vec<u8>)>> {
        self.buf.clear();
        self.port.write(packet)?;
        let until = Instant::now() + ANSWER;
        while let Some(left) = until.checked_duration_since(Instant::now()) {
            if self.port.wait(left)? {
                self.port.read(&mut self.buf)?;
                if let Some((from, error, params)) = status(&self.buf) {
                    if from == id {
                        return Ok(Some((error, params)));
                    }
                }
            }
        }
        Ok(None)
    }

    fn ping(&mut self, id: u8) -> io::Result<bool> {
        Ok(self.ask(&packet(id, instruction::PING, &[]), id)?.is_some())
    }

    /// The devices read each cycle: the IMU board first, as Pollen's
    /// runtime reads them, then the servos.
    fn devices(&self) -> Vec<u8> {
        self.imu.iter().map(|(id, _)| *id).chain(self.ids.iter().copied()).collect()
    }

    /// One Fast Sync Read of every device's block.
    fn read_blocks(&mut self) -> io::Result<Option<Vec<Part>>> {
        let devices = self.devices();
        self.buf.clear();
        self.port.write(&fast_sync_read(addr::BLOCK, addr::BLOCK_LEN, &devices))?;
        let until = Instant::now() + ANSWER;
        while let Some(left) = until.checked_duration_since(Instant::now()) {
            if self.port.wait(left)? {
                self.port.read(&mut self.buf)?;
                if let Some(parts) = fast_reply(&self.buf, devices.len(), addr::BLOCK_LEN as usize)? {
                    return Ok(Some(parts));
                }
            }
        }
        Ok(None)
    }

    fn write_all(&self, address: u16, value: &[u8]) -> io::Result<()> {
        let writes: Vec<(u8, Vec<u8>)> = self.ids.iter().map(|&id| (id, value.to_vec())).collect();
        self.port.write(&sync_write(address, &writes))
    }
}

impl Joints for DynamixelJoints {
    fn count(&self) -> usize {
        self.ids.len()
    }

    /// Whether each device answers a ping: a warning, not a failure, if
    /// one doesn't, since the robot (or drydock) may just not be started.
    fn check(&mut self) -> io::Result<()> {
        use keel::check::{report, Level};
        let what = format!("servos on {}", self.port.path());
        let mut silent = Vec::new();
        for id in self.devices() {
            if !self.ping(id)? {
                silent.push(id.to_string());
            }
        }
        match silent.len() {
            0 => report(Level::Ok, &what, &format!("all {} answer", self.devices().len())),
            _ => report(
                Level::Warn,
                &what,
                &format!("no answer from {}: start the robot, or drydock, before running", silent.join(", ")),
            ),
        }
        Ok(())
    }

    /// Waits for every device to answer, then holds each servo where it is:
    /// its goal its present position, its P gain set, torque on.
    fn start(&mut self) -> io::Result<()> {
        for id in self.devices() {
            let mut waited = false;
            while !self.ping(id)? {
                if !waited {
                    println!("waiting for device {id} on {}: is the robot, or drydock, running?", self.port.path());
                    waited = true;
                }
            }
            if waited {
                println!("device {id} answers");
            }
        }
        let parts = self.read_blocks()?.ok_or_else(|| {
            io::Error::other("the servos answer pings, but not a Fast Sync Read: their firmware may be older than v46")
        })?;
        let skip = self.imu.is_some() as usize;
        let goals: Vec<(u8, Vec<u8>)> =
            parts[skip..].iter().map(|p| (p.id, goal(state(p).position).to_le_bytes().to_vec())).collect();
        self.port.write(&sync_write(addr::GOAL_POSITION, &goals))?;
        self.write_all(addr::POSITION_P_GAIN, &self.p_gain.to_le_bytes())?;
        self.write_all(addr::TORQUE_ENABLE, &[1])?;
        println!(
            "{} servos holding where they are, P gain {}{}",
            self.ids.len(),
            self.p_gain,
            if self.imu.is_some() { ", and the IMU" } else { "" }
        );
        Ok(())
    }

    /// One Fast Sync Read: every servo's state, and the IMU's reading, as
    /// of when it was asked.
    fn read(&mut self, states: &mut [State]) -> io::Result<Option<u64>> {
        let stamp = now_ns();
        let Some(parts) = self.read_blocks()? else { return Ok(None) };
        let mut parts = parts.iter();
        if let Some((_, mount)) = self.imu {
            self.reading = parts.next().map(|p| imu(&p.data, mount));
        }
        for (s, p) in states.iter_mut().zip(parts) {
            *s = state(p);
        }
        Ok(Some(stamp))
    }

    /// The goals, in one Sync Write. Nothing until the controller has sent
    /// a command: the servos hold.
    fn write(&mut self, commands: Option<&[Command]>, _states: &[State]) -> io::Result<()> {
        let Some(commands) = commands else { return Ok(()) };
        if !self.mentioned
            && commands.iter().any(|c| c.velocity != 0.0 || c.kp != 0.0 || c.kd != 0.0 || c.effort != 0.0)
        {
            println!("servos take a position only: a command's velocity, gains and effort aren't used");
            self.mentioned = true;
        }
        let goals: Vec<(u8, Vec<u8>)> =
            self.ids.iter().zip(commands).map(|(&id, c)| (id, goal(c.position).to_le_bytes().to_vec())).collect();
        self.port.write(&sync_write(addr::GOAL_POSITION, &goals))
    }

    fn stop(&mut self) -> io::Result<()> {
        self.write_all(addr::TORQUE_ENABLE, &[0])
    }

    fn also_publish(&mut self) -> Vec<(&'static str, Vec<u8>)> {
        match self.reading.take() {
            Some(imu) => {
                let mut payload = vec![0; Imu::LEN];
                imu.write(&mut payload);
                vec![("imu", payload)]
            }
            None => vec![],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packets_match_the_specs_examples() {
        // Protocol 2.0's ping example: id 1.
        assert_eq!(packet(1, instruction::PING, &[]), [0xFF, 0xFF, 0xFD, 0x00, 0x01, 0x03, 0x00, 0x01, 0x19, 0x4E]);
        // Its Fast Sync Read example: Present Position (4 bytes at 132) from 3, 7 and 4.
        assert_eq!(
            fast_sync_read(132, 4, &[3, 7, 4]),
            [0xFF, 0xFF, 0xFD, 0x00, 0xFE, 0x0A, 0x00, 0x8A, 0x84, 0x00, 0x04, 0x00, 0x03, 0x07, 0x04, 0x20, 0xF2]
        );
    }

    #[test]
    fn stuffing_keeps_a_header_out_of_a_packet() {
        let p = packet(1, instruction::WRITE, &[0x00, 0x00, 0xFF, 0xFF, 0xFD]);
        assert_eq!(&p[8..14], [0x00, 0x00, 0xFF, 0xFF, 0xFD, 0xFD]);
        assert_eq!(u16::from_le_bytes([p[5], p[6]]), 9);
    }

    #[test]
    fn a_fast_reply_reads_device_by_device() {
        // The spec's answer to its example: positions 166, 2079 and 1023.
        let reply = [
            0xFF, 0xFF, 0xFD, 0x00, 0xFE, 0x19, 0x00, 0x55, //
            0x00, 0x03, 0xA6, 0x00, 0x00, 0x00, 0x84, 0x08, //
            0x00, 0x07, 0x1F, 0x08, 0x00, 0x00, 0x16, 0xCA, //
            0x00, 0x04, 0xFF, 0x03, 0x00, 0x00, 0xD1, 0x9E,
        ];
        let parts = fast_reply(&reply, 3, 4).unwrap().unwrap();
        assert_eq!(parts.iter().map(|p| p.id).collect::<Vec<_>>(), [3, 7, 4]);
        assert_eq!(parts[1].data, 2079u32.to_le_bytes());
        assert!(fast_reply(&reply[..reply.len() - 1], 3, 4).unwrap().is_none(), "not all there yet");
        let mut bad = reply;
        bad[12] ^= 1;
        assert!(fast_reply(&bad, 3, 4).is_err(), "a bad CRC");
    }

    #[test]
    fn positions_count_from_the_horns_centre() {
        assert_eq!(goal(0.0), 2048);
        assert_eq!(goal(std::f64::consts::FRAC_PI_2), 3072);
        assert_eq!(goal(10.0), 4095);
        let mut data = vec![0u8; 12];
        data[8..12].copy_from_slice(&3072i32.to_le_bytes());
        data[2..4].copy_from_slice(&500i16.to_le_bytes());
        let s = state(&Part { id: 1, error: ALERT, data });
        assert!((s.position - std::f64::consts::FRAC_PI_2).abs() < 1e-9 && (s.effort - 0.183).abs() < 1e-9);
        assert_eq!(s.status.flags, Status::DRIVE_ERROR);
    }

    #[test]
    fn the_imus_block_turns_into_the_bodys_frame() {
        // The zero3 board's mount (+90° about y), the body level and still:
        // the board's quaternion is the mount itself.
        let s = std::f64::consts::FRAC_1_SQRT_2;
        let mut block = vec![0u8; 12];
        block[8..10].copy_from_slice(&0x39A8u16.to_le_bytes()); // y ≈ 0.7071
        block[0..2].copy_from_slice(&(-57i16).to_le_bytes()); // -1 rad/s about the board's x
        let imu = imu(&block, [s, 0.0, s, 0.0]);
        assert!((imu.orientation[0].abs() - 1.0).abs() < 1e-3, "{:?}", imu.orientation);
        // The board's x is the body's -z, turned +90° about y.
        assert!((imu.angular_velocity[2] - 0.0175 * 57.0 * TAU / 360.0).abs() < 1e-9, "{:?}", imu.angular_velocity);
        assert!(imu.linear_acceleration[0].is_nan());
    }
}
