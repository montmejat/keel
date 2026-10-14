//! A CAN bus, through the kernel's SocketCAN: a raw socket bound to an
//! interface (`can0`, or the virtual `vcan0`), exchanging 16-byte frames.
//!
//! Also the frames `keel-can` speaks, in the absence of a real drive to
//! follow: a state frame per joint from the drive, a command frame per joint
//! to it; and [`CanJoints`], the joints at the other end, as a device.

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use crate::device::Joints;
use crate::{Command, Mode, State};

/// A classic CAN frame: an 11-bit id and up to 8 bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Frame {
    pub id: u32,
    pub len: u8,
    pub data: [u8; 8],
}

/// The kernel's `struct can_frame`.
#[repr(C)]
#[derive(Default)]
struct RawFrame {
    id: u32,
    len: u8,
    _pad: [u8; 3],
    data: [u8; 8],
}

const RAW_LEN: usize = std::mem::size_of::<RawFrame>();

pub struct Bus {
    socket: OwnedFd,
}

impl Bus {
    pub fn open(interface: &str) -> io::Result<Self> {
        let name = CString::new(interface)?;
        // SAFETY: plain syscalls on a socket we own, with an address that
        // lives through the call.
        unsafe {
            let index = libc::if_nametoindex(name.as_ptr());
            if index == 0 {
                return Err(io::Error::other(format!("no CAN interface `{interface}`")));
            }
            let fd = libc::socket(libc::PF_CAN, libc::SOCK_RAW | libc::SOCK_CLOEXEC, libc::CAN_RAW);
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let socket = OwnedFd::from_raw_fd(fd);
            let mut address: libc::sockaddr_can = std::mem::zeroed();
            address.can_family = libc::AF_CAN as _;
            address.can_ifindex = index as _;
            let len = std::mem::size_of::<libc::sockaddr_can>() as libc::socklen_t;
            if libc::bind(fd, (&raw const address).cast(), len) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self { socket })
        }
    }

    pub fn send(&self, frame: &Frame) -> io::Result<()> {
        let raw = RawFrame { id: frame.id, len: frame.len.min(8), data: frame.data, ..Default::default() };
        // SAFETY: `raw` is `RAW_LEN` readable bytes.
        let sent = unsafe { libc::write(self.socket.as_raw_fd(), (&raw const raw).cast(), RAW_LEN) };
        match sent {
            n if n == RAW_LEN as isize => Ok(()),
            _ => Err(io::Error::last_os_error()),
        }
    }

    /// The next frame if one is waiting. Frames this socket sent don't come
    /// back to it.
    pub fn try_recv(&self) -> io::Result<Option<Frame>> {
        let mut raw = RawFrame::default();
        // SAFETY: `raw` is `RAW_LEN` writable bytes, and any bytes are a
        // valid `RawFrame`.
        let got = unsafe { libc::recv(self.socket.as_raw_fd(), (&raw mut raw).cast(), RAW_LEN, libc::MSG_DONTWAIT) };
        match got {
            n if n == RAW_LEN as isize => Ok(Some(Frame { id: raw.id, len: raw.len.min(8), data: raw.data })),
            _ => match io::Error::last_os_error() {
                e if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
                e => Err(e),
            },
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

/// Command for joint `n`: id `COMMAND + n`, the value as an `f32`, then
/// its mode in a byte.
pub const COMMAND: u32 = 0x100;
/// State of joint `n`: id `STATE + n`, position then velocity as `f32`s.
pub const STATE: u32 = 0x180;
/// Joints a bus can carry before the two id ranges meet.
pub const MAX_JOINTS: usize = (STATE - COMMAND) as usize;

pub fn command_frame(joint: usize, command: Command) -> Frame {
    let mut data = [0; 8];
    data[..4].copy_from_slice(&(command.value as f32).to_le_bytes());
    data[4] = command.mode as u8;
    Frame { id: COMMAND + joint as u32, len: 5, data }
}

pub fn state_frame(joint: usize, state: State) -> Frame {
    let mut data = [0; 8];
    data[..4].copy_from_slice(&(state.position as f32).to_le_bytes());
    data[4..].copy_from_slice(&(state.velocity as f32).to_le_bytes());
    Frame { id: STATE + joint as u32, len: 8, data }
}

/// The joint and command a frame carries, if it's a command for one of
/// `joints` joints.
pub fn as_command(frame: &Frame, joints: usize) -> Option<(usize, Command)> {
    let joint = frame.id.checked_sub(COMMAND)? as usize;
    let mode = Mode::from_u8(frame.data[4])?;
    (joint < joints && frame.len == 5).then(|| (joint, Command { mode, value: f32_at(&frame.data, 0).into() }))
}

pub fn as_state(frame: &Frame, joints: usize) -> Option<(usize, State)> {
    let joint = frame.id.checked_sub(STATE)? as usize;
    let state = State { position: f32_at(&frame.data, 0).into(), velocity: f32_at(&frame.data, 4).into() };
    (joint < joints && frame.len == 8).then_some((joint, state))
}

/// Joints on a bus: what their drives last reported, and commands to them.
/// The drives `keel-can-motor` plays close every loop themselves.
pub struct CanJoints {
    bus: Bus,
    states: Vec<State>,
    /// Which joints have reported since the last complete state.
    fresh: Vec<bool>,
}

impl CanJoints {
    pub fn open(interface: &str, joints: usize) -> io::Result<CanJoints> {
        if joints > MAX_JOINTS {
            return Err(io::Error::other(format!("a bus carries at most {MAX_JOINTS} joints")));
        }
        Ok(CanJoints { bus: Bus::open(interface)?, states: vec![State::default(); joints], fresh: vec![false; joints] })
    }
}

impl Joints for CanJoints {
    fn count(&self) -> usize {
        self.states.len()
    }

    /// A state is every joint's: nothing is published from half a bus.
    fn read(&mut self, states: &mut [State]) -> io::Result<Option<u64>> {
        let read = keel::trace::now_ns();
        while let Some(frame) = self.bus.try_recv()? {
            if let Some((joint, state)) = as_state(&frame, self.states.len()) {
                (self.states[joint], self.fresh[joint]) = (state, true);
            }
        }
        if !self.fresh.iter().all(|&f| f) {
            return Ok(None);
        }
        self.fresh.fill(false);
        states.copy_from_slice(&self.states);
        Ok(Some(read))
    }

    fn write(&mut self, commands: &[Command]) -> io::Result<()> {
        for (joint, command) in commands.iter().enumerate() {
            self.bus.send(&command_frame(joint, *command))?;
        }
        Ok(())
    }

    fn closes(&self, _mode: Mode) -> bool {
        true
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
        let state = State { position: 1.5, velocity: -0.25 };
        assert_eq!(as_state(&state_frame(2, state), 3), Some((2, state)));
        assert_eq!(as_state(&state_frame(2, state), 2), None, "no such joint");
        assert_eq!(as_command(&state_frame(2, state), 3), None, "not a command");
        for command in [Command::effort(-4.0), Command::position(0.5)] {
            assert_eq!(as_command(&command_frame(0, command), 1), Some((0, command)));
        }
    }

    /// Needs a `vcan0`; without root:
    /// `unshare -rn sh -c 'ip link add vcan0 type vcan && ip link set vcan0 up && cargo test -p keel-control -- --ignored'`
    #[test]
    #[ignore]
    fn frames_cross_a_bus() {
        let (a, b) = (Bus::open("vcan0").unwrap(), Bus::open("vcan0").unwrap());
        assert!(b.try_recv().unwrap().is_none());
        let frame = state_frame(1, State { position: 0.5, velocity: 2.0 });
        a.send(&frame).unwrap();
        assert_eq!(b.try_recv().unwrap(), Some(frame));
        assert!(a.try_recv().unwrap().is_none(), "not its own");
        assert!(Bus::open("nope0").is_err());
    }
}
