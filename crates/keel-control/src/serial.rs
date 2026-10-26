//! A serial port, raw: bytes as they come, at a baud rate, through the
//! system calls (`termios`), as `can` does SocketCAN.

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::Duration;

pub struct Port {
    fd: OwnedFd,
    path: String,
}

/// The rates a port takes, and their termios constants.
const RATES: &[(u32, libc::speed_t)] = &[
    (9600, libc::B9600),
    (57600, libc::B57600),
    (115200, libc::B115200),
    (1_000_000, libc::B1000000),
    (2_000_000, libc::B2000000),
    (3_000_000, libc::B3000000),
    (4_000_000, libc::B4000000),
];

impl Port {
    /// `path` at `baud`, raw: no echo, no line editing, eight data bits.
    pub fn open(path: &str, baud: u32) -> io::Result<Self> {
        let speed = RATES.iter().find(|(b, _)| *b == baud).map(|(_, s)| *s).ok_or_else(|| {
            let rates: Vec<String> = RATES.iter().map(|(b, _)| b.to_string()).collect();
            io::Error::other(format!("--baud {baud}: a serial port here runs at {}", rates.join(", ")))
        })?;
        let c_path = CString::new(path)?;
        // SAFETY: plain system calls on a descriptor we own, with arguments
        // that live through each call.
        unsafe {
            let fd = libc::open(c_path.as_ptr(), libc::O_RDWR | libc::O_NOCTTY | libc::O_NONBLOCK | libc::O_CLOEXEC);
            if fd < 0 {
                let e = io::Error::last_os_error();
                return Err(io::Error::new(e.kind(), format!("can't open the serial port {path}: {e}")));
            }
            let port = Port { fd: OwnedFd::from_raw_fd(fd), path: path.to_owned() };
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut t) != 0 {
                return Err(io::Error::other(format!("{path} isn't a serial port: {}", io::Error::last_os_error())));
            }
            libc::cfmakeraw(&mut t);
            libc::cfsetispeed(&mut t, speed);
            libc::cfsetospeed(&mut t, speed);
            t.c_cflag |= libc::CLOCAL | libc::CREAD;
            if libc::tcsetattr(fd, libc::TCSANOW, &t) != 0 {
                return Err(io::Error::last_os_error());
            }
            libc::tcflush(fd, libc::TCIOFLUSH);
            Ok(port)
        }
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    /// All of `bytes`, waiting while the port's buffer is full.
    pub fn write(&self, mut bytes: &[u8]) -> io::Result<()> {
        while !bytes.is_empty() {
            // SAFETY: `bytes` lives through the call.
            let n = unsafe { libc::write(self.fd.as_raw_fd(), bytes.as_ptr().cast(), bytes.len()) };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::WouldBlock {
                    self.wait_for(libc::POLLOUT, Duration::from_millis(10))?;
                    continue;
                }
                return Err(e);
            }
            bytes = &bytes[n as usize..];
        }
        Ok(())
    }

    /// What's there to read, appended to `buf`; nothing if nothing.
    pub fn read(&self, buf: &mut Vec<u8>) -> io::Result<()> {
        let mut chunk = [0u8; 1024];
        loop {
            // SAFETY: `chunk` lives through the call.
            let n = unsafe { libc::read(self.fd.as_raw_fd(), chunk.as_mut_ptr().cast(), chunk.len()) };
            if n < 0 {
                let e = io::Error::last_os_error();
                return if e.kind() == io::ErrorKind::WouldBlock { Ok(()) } else { Err(e) };
            }
            if n == 0 {
                return Ok(());
            }
            buf.extend_from_slice(&chunk[..n as usize]);
        }
    }

    /// Waits up to `timeout` for bytes to read; whether there are.
    pub fn wait(&self, timeout: Duration) -> io::Result<bool> {
        self.wait_for(libc::POLLIN, timeout)
    }

    fn wait_for(&self, events: libc::c_short, timeout: Duration) -> io::Result<bool> {
        let mut p = libc::pollfd { fd: self.fd.as_raw_fd(), events, revents: 0 };
        let ts = libc::timespec { tv_sec: timeout.as_secs() as _, tv_nsec: timeout.subsec_nanos() as _ };
        // SAFETY: `p` and `ts` live through the call.
        let n = unsafe { libc::ppoll(&mut p, 1, &ts, std::ptr::null()) };
        if n < 0 {
            let e = io::Error::last_os_error();
            return if e.kind() == io::ErrorKind::Interrupted { Ok(false) } else { Err(e) };
        }
        Ok(n > 0 && p.revents & events != 0)
    }
}
