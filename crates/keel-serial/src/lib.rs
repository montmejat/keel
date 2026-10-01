//! A serial port as a `keel_micro::Link`: the machine's end of the link to a
//! microcontroller (`/dev/ttyACM0`, `/dev/ttyUSB0`), or, to try things
//! without one, the chip's end of a pseudo-terminal.

use std::ffi::{CStr, CString};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use keel_micro::Link;

pub struct Port {
    fd: OwnedFd,
    /// Bytes read and not yet handed out: `buffer[start..end]`.
    buffer: [u8; 256],
    start: usize,
    end: usize,
}

impl Port {
    /// Opens a serial device at `baud` (one of the usual rates), raw: bytes
    /// go through as they are.
    pub fn open(device: &Path, baud: u32) -> io::Result<Self> {
        let path = CString::new(device.as_os_str().as_bytes())?;
        // SAFETY: plain syscall with a string that lives through it.
        let fd =
            unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_NOCTTY | libc::O_NONBLOCK | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(os_error(&device.display().to_string()));
        }
        // SAFETY: we own the descriptor we just opened.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let speed = match baud {
            9600 => libc::B9600,
            57600 => libc::B57600,
            115200 => libc::B115200,
            230400 => libc::B230400,
            460800 => libc::B460800,
            921600 => libc::B921600,
            1000000 => libc::B1000000,
            _ => return Err(io::Error::other(format!("unsupported baud rate {baud}"))),
        };
        raw(fd.as_raw_fd(), Some(speed))?;
        Ok(Self::new(fd))
    }

    /// Makes a pseudo-terminal and returns the chip's end of it; the other
    /// end appears at `device`, for [`Port::open`], as a real chip's would in
    /// `/dev`.
    pub fn pretend(device: &Path) -> io::Result<Self> {
        // SAFETY: plain syscalls on the descriptor we get, and `name` is
        // large enough for `ptsname_r` to terminate.
        let (fd, other_end) = unsafe {
            let fd = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_NONBLOCK | libc::O_CLOEXEC);
            if fd < 0 {
                return Err(os_error("posix_openpt"));
            }
            let fd = OwnedFd::from_raw_fd(fd);
            let mut name = [0 as libc::c_char; 128];
            if libc::grantpt(fd.as_raw_fd()) != 0
                || libc::unlockpt(fd.as_raw_fd()) != 0
                || libc::ptsname_r(fd.as_raw_fd(), name.as_mut_ptr(), name.len()) != 0
            {
                return Err(os_error("pseudo-terminal"));
            }
            (fd, CStr::from_ptr(name.as_ptr()).to_string_lossy().into_owned())
        };
        // Before anyone opens the other end: no echo of what we send.
        raw(fd.as_raw_fd(), None)?;
        let _ = std::fs::remove_file(device);
        std::os::unix::fs::symlink(other_end, device)?;
        Ok(Self::new(fd))
    }

    fn new(fd: OwnedFd) -> Self {
        Self { fd, buffer: [0; 256], start: 0, end: 0 }
    }
}

fn os_error(what: &str) -> io::Error {
    let e = io::Error::last_os_error();
    io::Error::new(e.kind(), format!("{what}: {e}"))
}

/// No line editing, no echo, no translation: a terminal as a byte pipe.
fn raw(fd: i32, speed: Option<libc::speed_t>) -> io::Result<()> {
    // SAFETY: `termios` is filled by `tcgetattr` before it's used.
    unsafe {
        let mut termios: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut termios) != 0 {
            return Err(os_error("not a serial port"));
        }
        libc::cfmakeraw(&mut termios);
        if let Some(speed) = speed {
            libc::cfsetspeed(&mut termios, speed);
        }
        if libc::tcsetattr(fd, libc::TCSANOW, &termios) != 0 {
            return Err(os_error("tcsetattr"));
        }
    }
    Ok(())
}

impl Link for Port {
    fn read(&mut self) -> Option<u8> {
        if self.start == self.end {
            // SAFETY: the buffer is writable for its length.
            let got = unsafe { libc::read(self.fd.as_raw_fd(), self.buffer.as_mut_ptr().cast(), self.buffer.len()) };
            if got <= 0 {
                return None;
            }
            (self.start, self.end) = (0, got as usize);
        }
        self.start += 1;
        Some(self.buffer[self.start - 1])
    }

    /// What the port can't take now is dropped, as on a chip whose transmit
    /// buffer is full: the frame it belonged to fails its CRC at the other
    /// end.
    fn write(&mut self, bytes: &[u8]) {
        // SAFETY: `bytes` is readable for its length.
        unsafe { libc::write(self.fd.as_raw_fd(), bytes.as_ptr().cast(), bytes.len()) };
    }
}
