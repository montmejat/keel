//! keel as process 1: the only program on the machine.
//!
//! The kernel unpacks an image made by `keel image` into memory and runs its
//! `/init`, which is `keel`. With no distribution underneath, what one would
//! have done falls to us, and it's short:
//!
//! 1. mount `/proc`, `/sys`, `/dev`, and the memory file systems keel uses;
//! 2. load the kernel modules the image carries (a network driver);
//! 3. give the machine its address, from the kernel command line:
//!    `keel.ip=10.0.2.15/24 keel.gateway=10.0.2.2 keel.listen=0.0.0.0:7400`
//!    `keel.name=robot-1`;
//! 4. run `keel daemon`, and run it again if it dies. It runs as root, so
//!    its nodes can have real-time priority and lock their memory;
//! 5. reap every process that loses its parent, which is process 1's job.
//!
//! Process 1 must never exit (the kernel panics), so nothing here is fatal:
//! what fails is reported on the console and the rest goes on.

use std::ffi::CString;
use std::fs;
use std::io;
use std::net::Ipv4Addr;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

/// Where the image keeps the modules to load, in order.
pub const MODULES: &str = "/modules";
/// The daemon's home: its token is there, and its store goes there.
pub const HOME: &str = "/root";

/// Where the daemon listens unless `keel.listen=` says otherwise: the
/// machine exists to be reached.
const LISTEN: &str = "0.0.0.0:7400";
/// How long the network driver gets to show its interface.
const INTERFACE_TIMEOUT: Duration = Duration::from_secs(5);
/// Between a daemon dying and the next one.
const RESPAWN_DELAY: Duration = Duration::from_secs(1);

pub fn run() -> ! {
    report("mounting", mount_all());
    let cmdline = fs::read_to_string("/proc/cmdline").unwrap_or_default();
    let param = |name: &str| cmdline.split_whitespace().find_map(|p| p.strip_prefix(name)?.strip_prefix('='));
    say(format!("keel {} is process 1", env!("CARGO_PKG_VERSION")));

    report("loading modules", load_modules());
    if let Some(name) = param("keel.name") {
        // SAFETY: the pointer and length describe `name`.
        unsafe { libc::sethostname(name.as_ptr().cast(), name.len()) };
    }
    report("network", network(param("keel.ip"), param("keel.gateway")));
    // Real-time nodes lock their memory; the daemon and its nodes inherit this.
    let unlimited = libc::rlimit { rlim_cur: libc::RLIM_INFINITY, rlim_max: libc::RLIM_INFINITY };
    // SAFETY: plain syscall with a valid limit.
    unsafe { libc::setrlimit(libc::RLIMIT_MEMLOCK, &unlimited) };

    let listen = param("keel.listen").unwrap_or(LISTEN);
    let exe = std::env::current_exe().unwrap_or_else(|_| "/init".into());
    loop {
        let mut command = Command::new(&exe);
        command.args(["daemon", "--listen", listen]).env("HOME", HOME).env("XDG_RUNTIME_DIR", "/run");
        match command.spawn() {
            Ok(daemon) => {
                let status = reap_until(daemon.id() as i32);
                say(format!("the daemon exited (status {status:#x}), starting it again"));
            }
            Err(e) => say(format!("can't start the daemon: {e}")),
        }
        std::thread::sleep(RESPAWN_DELAY);
    }
}

fn say(text: String) {
    println!("[init] {text}");
}

fn report(what: &str, result: io::Result<()>) {
    if let Err(e) = result {
        say(format!("{what}: {e}"));
    }
}

/// Waits for `pid` to exit, reaping whatever else exits meanwhile: orphans
/// are handed to process 1.
fn reap_until(pid: i32) -> i32 {
    loop {
        let mut status = 0;
        // SAFETY: plain syscall.
        let exited = unsafe { libc::wait(&mut status) };
        if exited == pid || (exited < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD)) {
            return status;
        }
    }
}

fn mount_all() -> io::Result<()> {
    for (source, target, kind) in [
        ("proc", "/proc", "proc"),
        ("sysfs", "/sys", "sysfs"),
        ("devtmpfs", "/dev", "devtmpfs"),
        // Shared-memory regions.
        ("tmpfs", "/dev/shm", "tmpfs"),
        // Sockets.
        ("tmpfs", "/run", "tmpfs"),
        ("tmpfs", "/tmp", "tmpfs"),
    ] {
        fs::create_dir_all(target)?;
        let (source, c_target, kind) = (CString::new(source)?, CString::new(target)?, CString::new(kind)?);
        // SAFETY: the three strings live through the call.
        let mounted = unsafe { libc::mount(source.as_ptr(), c_target.as_ptr(), kind.as_ptr(), 0, std::ptr::null()) };
        if mounted != 0 {
            return Err(os_error(target));
        }
    }
    Ok(())
}

fn os_error(what: &str) -> io::Error {
    let e = io::Error::last_os_error();
    io::Error::new(e.kind(), format!("{what}: {e}"))
}

/// Loads every file in [`MODULES`], in name order: `keel image` numbered
/// them so that each comes after what it needs.
fn load_modules() -> io::Result<()> {
    let Ok(entries) = fs::read_dir(MODULES) else { return Ok(()) };
    let mut modules: Vec<_> = entries.flatten().map(|e| e.path()).collect();
    modules.sort();
    for module in modules {
        let file = fs::File::open(&module)?;
        // SAFETY: an open file, and an empty parameter string.
        let loaded = unsafe { libc::syscall(libc::SYS_finit_module, file.as_raw_fd(), c"".as_ptr(), 0) };
        if loaded != 0 {
            return Err(os_error(&module.display().to_string()));
        }
    }
    Ok(())
}

/// Brings up the loopback, and the first other interface with `ip`
/// (`address/prefix`), routing everything else through `gateway`.
fn network(ip: Option<&str>, gateway: Option<&str>) -> io::Result<()> {
    // SAFETY: plain syscall; the socket is only a handle for ioctls, and
    // lives as long as the machine.
    let socket = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
    if socket < 0 {
        return Err(os_error("socket"));
    }
    set_flags(socket, "lo")?;
    let Some(ip) = ip else {
        say("no keel.ip= on the kernel command line: loopback only".into());
        return Ok(());
    };
    let (address, prefix) = ip.split_once('/').unwrap_or((ip, "24"));
    let bad = |what: &str| io::Error::other(format!("can't read `{what}`"));
    let address: Ipv4Addr = address.parse().map_err(|_| bad(ip))?;
    let prefix: u32 = prefix.parse().ok().filter(|p| *p <= 32).ok_or_else(|| bad(ip))?;
    let mask = Ipv4Addr::from(u32::MAX.checked_shl(32 - prefix).unwrap_or(0));

    let interface = wait_for_interface()?;
    set_address(socket, &interface, libc::SIOCSIFADDR, address)?;
    set_address(socket, &interface, libc::SIOCSIFNETMASK, mask)?;
    set_flags(socket, &interface)?;
    if let Some(gateway) = gateway {
        let gateway: Ipv4Addr = gateway.parse().map_err(|_| bad(gateway))?;
        // SAFETY: an all-zero `rtentry` is the default route to nowhere; we
        // fill in the gateway.
        let mut route: libc::rtentry = unsafe { std::mem::zeroed() };
        route.rt_dst = sockaddr(Ipv4Addr::UNSPECIFIED);
        route.rt_genmask = sockaddr(Ipv4Addr::UNSPECIFIED);
        route.rt_gateway = sockaddr(gateway);
        route.rt_flags = libc::RTF_UP | libc::RTF_GATEWAY;
        // SAFETY: `route` is a valid `rtentry` for the call.
        if unsafe { libc::ioctl(socket, libc::SIOCADDRT as _, &route) } != 0 {
            return Err(os_error("default route"));
        }
    }
    say(format!("{interface} is {address}/{prefix}"));
    Ok(())
}

/// The first interface that isn't the loopback, once its driver has made it.
fn wait_for_interface() -> io::Result<String> {
    let deadline = Instant::now() + INTERFACE_TIMEOUT;
    loop {
        let found = (fs::read_dir(Path::new("/sys/class/net"))?.flatten())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name != "lo")
            .min();
        match found {
            Some(interface) => return Ok(interface),
            None if Instant::now() > deadline => return Err(io::Error::other("no network interface (no driver?)")),
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

fn sockaddr(address: Ipv4Addr) -> libc::sockaddr {
    let v4 = libc::sockaddr_in {
        sin_family: libc::AF_INET as _,
        sin_port: 0,
        sin_addr: libc::in_addr { s_addr: u32::from(address).to_be() },
        sin_zero: [0; 8],
    };
    // SAFETY: `sockaddr_in` is a `sockaddr`, of the same size.
    unsafe { std::mem::transmute(v4) }
}

fn request(interface: &str) -> libc::ifreq {
    // SAFETY: all zeroes is a valid `ifreq`.
    let mut request: libc::ifreq = unsafe { std::mem::zeroed() };
    for (to, from) in request.ifr_name.iter_mut().zip(interface.bytes().take(libc::IFNAMSIZ - 1)) {
        *to = from as _;
    }
    request
}

fn set_address(socket: i32, interface: &str, what: libc::c_ulong, address: Ipv4Addr) -> io::Result<()> {
    let mut request = request(interface);
    request.ifr_ifru.ifru_addr = sockaddr(address);
    // SAFETY: `request` is a valid `ifreq` for the call.
    match unsafe { libc::ioctl(socket, what as _, &request) } {
        0 => Ok(()),
        _ => Err(os_error(interface)),
    }
}

/// Brings an interface up.
fn set_flags(socket: i32, interface: &str) -> io::Result<()> {
    let mut request = request(interface);
    // SAFETY: `request` is a valid `ifreq`, read then written by the calls;
    // the flags are what the first call stored in the union.
    unsafe {
        if libc::ioctl(socket, libc::SIOCGIFFLAGS as _, &mut request) != 0 {
            return Err(os_error(interface));
        }
        request.ifr_ifru.ifru_flags |= (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short;
        if libc::ioctl(socket, libc::SIOCSIFFLAGS as _, &request) != 0 {
            return Err(os_error(interface));
        }
    }
    Ok(())
}
