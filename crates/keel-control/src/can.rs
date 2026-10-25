//! A CAN bus, through the kernel's SocketCAN: a raw socket bound to an
//! interface (`can0`, or the virtual `vcan0`), exchanging classic frames, and
//! CAN FD frames if asked to.
//!
//! Also the made-up frames `keel-can` speaks to `keel-can-motor`: a state
//! frame per joint from the drive, a torque frame per joint to it. Real
//! drives' frames are elsewhere (`damiao`).

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use keel::trace::now_ns;

use crate::joints::Joints;
use crate::{Command, State};

/// A frame with an 11-bit id: classic (up to 8 bytes), or CAN FD (up to 64,
/// its data at the faster bit rate).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame {
    pub id: u32,
    pub len: u8,
    pub data: [u8; 64],
    pub fd: bool,
}

impl Frame {
    /// A classic frame of `data.len()` bytes, 8 at most.
    pub fn new(id: u32, data: &[u8]) -> Self {
        let mut all = [0; 64];
        let len = data.len().min(8);
        all[..len].copy_from_slice(&data[..len]);
        Self { id, len: len as u8, data: all, fd: false }
    }

    /// The same frame, as CAN FD or classic.
    pub fn fd(self, fd: bool) -> Self {
        Self { fd, ..self }
    }

    /// Its data, when it's 8 bytes long.
    pub fn eight(&self) -> Option<[u8; 8]> {
        (self.len == 8).then(|| self.data[..8].try_into().unwrap())
    }
}

/// The kernel's `struct canfd_frame`; a `struct can_frame` is its first 16
/// bytes.
#[repr(C)]
struct RawFrame {
    id: u32,
    len: u8,
    flags: u8,
    _reserved: [u8; 2],
    data: [u8; 64],
}

const CAN_MTU: usize = 16;
const CANFD_MTU: usize = std::mem::size_of::<RawFrame>();
/// A CAN FD frame's data at the faster bit rate.
const CANFD_BRS: u8 = 0x01;
/// Extended ids, remote requests and error frames.
const NOT_DATA_FRAME: u32 = 0x8000_0000 | 0x4000_0000 | 0x2000_0000;
const SOL_CAN_RAW: libc::c_int = libc::SOL_CAN_BASE + libc::CAN_RAW;
const CAN_RAW_FD_FRAMES: libc::c_int = 5;

pub struct Bus {
    socket: OwnedFd,
}

impl Bus {
    /// A socket for classic frames.
    pub fn open(interface: &str) -> io::Result<Self> {
        Self::open_with(interface, false)
    }

    /// A socket for classic and CAN FD frames: the interface must carry
    /// CAN FD.
    pub fn open_fd(interface: &str) -> io::Result<Self> {
        Self::open_with(interface, true)
    }

    fn open_with(interface: &str, fd: bool) -> io::Result<Self> {
        let name = CString::new(interface)?;
        // SAFETY: plain syscalls on a socket we own, with arguments that
        // live through each call.
        unsafe {
            let index = libc::if_nametoindex(name.as_ptr());
            if index == 0 {
                return Err(io::Error::other(format!("no CAN interface `{interface}`: {}", make(interface, fd))));
            }
            let raw = libc::socket(libc::PF_CAN, libc::SOCK_RAW | libc::SOCK_CLOEXEC, libc::CAN_RAW);
            if raw < 0 {
                return Err(io::Error::last_os_error());
            }
            let socket = OwnedFd::from_raw_fd(raw);
            if fd {
                let mut ifr: libc::ifreq = std::mem::zeroed();
                for (dst, &src) in ifr.ifr_name.iter_mut().zip(name.as_bytes()) {
                    *dst = src as libc::c_char;
                }
                if libc::ioctl(raw, libc::SIOCGIFMTU, &mut ifr) < 0 {
                    return Err(io::Error::last_os_error());
                }
                if ifr.ifr_ifru.ifru_mtu as usize != CANFD_MTU {
                    let fix = match interface.starts_with("vcan") {
                        true => format!("sudo ip link set {interface} down && sudo ip link set {interface} mtu 72 up"),
                        false => format!(
                            "sudo ip link set {interface} down && sudo ip link set {interface} type can bitrate \
                             1000000 dbitrate 5000000 fd on && sudo ip link set {interface} up"
                        ),
                    };
                    return Err(io::Error::other(format!(
                        "`{interface}` doesn't carry CAN FD frames (its MTU isn't 72): {fix}"
                    )));
                }
                let on: libc::c_int = 1;
                let size = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
                if libc::setsockopt(raw, SOL_CAN_RAW, CAN_RAW_FD_FRAMES, (&raw const on).cast(), size) < 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            let mut address: libc::sockaddr_can = std::mem::zeroed();
            address.can_family = libc::AF_CAN as _;
            address.can_ifindex = index as _;
            let len = std::mem::size_of::<libc::sockaddr_can>() as libc::socklen_t;
            if libc::bind(raw, (&raw const address).cast(), len) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self { socket })
        }
    }

    pub fn send(&self, frame: &Frame) -> io::Result<()> {
        let (len, size, flags) = match frame.fd {
            true => (frame.len.min(64), CANFD_MTU, CANFD_BRS),
            false => (frame.len.min(8), CAN_MTU, 0),
        };
        let raw = RawFrame { id: frame.id, len, flags, _reserved: [0; 2], data: frame.data };
        // SAFETY: `raw` has at least `size` readable bytes.
        let sent = unsafe { libc::write(self.socket.as_raw_fd(), (&raw const raw).cast(), size) };
        match sent {
            n if n == size as isize => Ok(()),
            _ => Err(io::Error::last_os_error()),
        }
    }

    /// The next data frame if one is waiting. Frames this socket sent don't
    /// come back to it; CAN FD frames come only to a socket opened for them.
    pub fn try_recv(&self) -> io::Result<Option<Frame>> {
        loop {
            let mut raw = RawFrame { id: 0, len: 0, flags: 0, _reserved: [0; 2], data: [0; 64] };
            // SAFETY: `raw` is `CANFD_MTU` writable bytes, and any bytes are
            // a valid `RawFrame`.
            let got =
                unsafe { libc::recv(self.socket.as_raw_fd(), (&raw mut raw).cast(), CANFD_MTU, libc::MSG_DONTWAIT) };
            let fd = match got {
                n if n == CAN_MTU as isize => false,
                n if n == CANFD_MTU as isize => true,
                n if n >= 0 => continue,
                _ => {
                    return match io::Error::last_os_error() {
                        e if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
                        e => Err(e),
                    }
                }
            };
            if raw.id & NOT_DATA_FRAME != 0 {
                continue;
            }
            return Ok(Some(Frame { id: raw.id, len: raw.len.min(if fd { 64 } else { 8 }), data: raw.data, fd }));
        }
    }

    /// Waits up to `timeout` for a frame to read; true if there's one.
    /// `ppoll`, not `poll`: a control cycle is shorter than its milliseconds.
    pub fn wait(&self, timeout: std::time::Duration) -> io::Result<bool> {
        let mut fd = libc::pollfd { fd: self.socket.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        let ts = libc::timespec { tv_sec: timeout.as_secs() as _, tv_nsec: timeout.subsec_nanos() as _ };
        // SAFETY: one valid pollfd, a valid timespec, no signal mask.
        match unsafe { libc::ppoll(&mut fd, 1, &ts, std::ptr::null()) } {
            -1 => match io::Error::last_os_error() {
                e if e.kind() == io::ErrorKind::Interrupted => Ok(false),
                e => Err(e),
            },
            n => Ok(n > 0),
        }
    }
}

/// How to make an interface that's missing: a virtual one for `vcan*`, a
/// real one's settings otherwise (1 Mbit/s, and 5 for CAN FD's data).
fn make(interface: &str, fd: bool) -> String {
    match (interface.starts_with("vcan"), fd) {
        (true, _) => format!("sudo ip link add dev {interface} type vcan && sudo ip link set {interface} mtu 72 up"),
        (false, true) => format!(
            "is the adapter plugged in? Then: sudo ip link set {interface} type can bitrate 1000000 dbitrate \
             5000000 fd on && sudo ip link set {interface} up"
        ),
        (false, false) => format!(
            "is the adapter plugged in? Then: sudo ip link set {interface} type can bitrate 1000000 && sudo ip \
             link set {interface} up"
        ),
    }
}

/// Torque for joint `n`: id `COMMAND + n`, as an `f32`.
pub const COMMAND: u32 = 0x100;
/// State of joint `n`: id `STATE + n`, position then velocity as `f32`s.
pub const STATE: u32 = 0x180;
/// Joints a bus can carry before the two id ranges meet.
pub const MAX_JOINTS: usize = (STATE - COMMAND) as usize;

pub fn command_frame(joint: usize, effort: f64) -> Frame {
    Frame::new(COMMAND + joint as u32, &(effort as f32).to_le_bytes())
}

pub fn state_frame(joint: usize, state: State) -> Frame {
    let mut data = [0; 8];
    data[..4].copy_from_slice(&(state.position as f32).to_le_bytes());
    data[4..].copy_from_slice(&(state.velocity as f32).to_le_bytes());
    Frame::new(STATE + joint as u32, &data)
}

/// The joint and torque a frame carries, if it's a torque for one of
/// `joints` joints.
pub fn as_command(frame: &Frame, joints: usize) -> Option<(usize, f64)> {
    let joint = frame.id.checked_sub(COMMAND)? as usize;
    (joint < joints && frame.len == 4).then(|| (joint, f32_at(&frame.data, 0).into()))
}

pub fn as_state(frame: &Frame, joints: usize) -> Option<(usize, State)> {
    let joint = frame.id.checked_sub(STATE)? as usize;
    let state = State {
        position: f32_at(&frame.data, 0).into(),
        velocity: f32_at(&frame.data, 4).into(),
        ..Default::default()
    };
    (joint < joints && frame.len == 8).then_some((joint, state))
}

/// Joints behind made-up drives (`keel-can-motor`'s): they report on their
/// own clock and take only a torque, so a command's law is worked out here,
/// on the latest state, once a cycle. Their frames don't carry the torque
/// they give, so a state's effort is the torque last sent.
pub struct CanJoints {
    bus: Bus,
    fresh: Vec<bool>,
    sent: Vec<f64>,
}

impl CanJoints {
    pub fn open(interface: &str, joints: usize) -> io::Result<Self> {
        if joints > MAX_JOINTS {
            return Err(io::Error::other(format!("a bus carries at most {MAX_JOINTS} joints")));
        }
        Ok(Self { bus: Bus::open(interface)?, fresh: vec![false; joints], sent: vec![0.0; joints] })
    }
}

impl Joints for CanJoints {
    fn count(&self) -> usize {
        self.fresh.len()
    }

    /// What the drives reported since the last read, stamped with when it
    /// was read.
    fn read(&mut self, states: &mut [State]) -> io::Result<Option<u64>> {
        let read_ns = now_ns();
        while let Some(frame) = self.bus.try_recv()? {
            if let Some((joint, state)) = as_state(&frame, self.fresh.len()) {
                let state = State { effort: self.sent[joint], ..state };
                (states[joint], self.fresh[joint]) = (state, true);
            }
        }
        if !self.fresh.iter().all(|&f| f) {
            return Ok(None);
        }
        self.fresh.fill(false);
        Ok(Some(read_ns))
    }

    fn write(&mut self, commands: Option<&[Command]>, states: &[State]) -> io::Result<()> {
        let Some(commands) = commands else { return Ok(()) };
        for (joint, (command, state)) in commands.iter().zip(states).enumerate() {
            self.sent[joint] = command.torque(*state);
            self.bus.send(&command_frame(joint, self.sent[joint]))?;
        }
        Ok(())
    }
}

fn f32_at(bytes: &[u8], at: usize) -> f32 {
    f32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let state = State { position: 1.5, velocity: -0.25, ..Default::default() };
        assert_eq!(as_state(&state_frame(2, state), 3), Some((2, state)));
        assert_eq!(as_state(&state_frame(2, state), 2), None, "no such joint");
        assert_eq!(as_command(&state_frame(2, state), 3), None, "not a command");
        assert_eq!(as_command(&command_frame(0, -4.0), 1), Some((0, -4.0)));
    }

    /// Needs a `vcan0`; without root:
    /// `unshare -rn sh -c 'ip link add vcan0 type vcan && ip link set vcan0 up && cargo test -p keel-control -- --ignored'`
    #[test]
    #[ignore]
    fn frames_cross_a_bus() {
        let (a, b) = (Bus::open("vcan0").unwrap(), Bus::open_fd("vcan0").unwrap());
        assert!(b.try_recv().unwrap().is_none());
        let frame = state_frame(1, State { position: 0.5, velocity: 2.0, ..Default::default() });
        a.send(&frame).unwrap();
        assert_eq!(b.try_recv().unwrap(), Some(frame));
        assert!(a.try_recv().unwrap().is_none(), "not its own");
        // CAN FD: to a socket opened for it, not to the classic one.
        let fd = Frame::new(0x01, &[1, 2, 3, 4, 5, 6, 7, 8]).fd(true);
        b.send(&fd).unwrap();
        assert!(a.try_recv().unwrap().is_none());
        assert!(Bus::open("nope0").is_err());
    }
}
