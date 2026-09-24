//! SIGINT and SIGTERM ask the daemon to stop the dataflow instead of killing
//! it on the spot. Nodes run in their own process group, so Ctrl-C in the
//! terminal reaches only the daemon, which then stops them in order.

use std::sync::atomic::{AtomicU32, Ordering};

static RECEIVED: AtomicU32 = AtomicU32::new(0);

extern "C" fn on_signal(_: libc::c_int) {
    // Only async-signal-safe work here: bump a counter the supervisor polls.
    RECEIVED.fetch_add(1, Ordering::Relaxed);
}

pub fn install() {
    for signal in [libc::SIGINT, libc::SIGTERM] {
        // SAFETY: the handler only touches an atomic.
        unsafe { libc::signal(signal, on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t) };
    }
}

/// How many stop signals have arrived.
pub fn received() -> u32 {
    RECEIVED.load(Ordering::Relaxed)
}
