//! Produces 1080p RGB frames at 30 Hz, drawn in place in shared memory. The
//! scene slowly brightens and darkens.
//!
//! It has no inputs, so it never sees `Stop`: the daemon ends it with SIGTERM.

use std::time::{Duration, Instant};

use keel::Node;
use pipeline::{FPS, FRAME_LEN, HEADER_LEN};

fn main() -> std::io::Result<()> {
    let mut node = Node::from_env()?;
    let period = Duration::from_secs(1) / FPS;
    let mut next = Instant::now();
    for frame in 0u64.. {
        let level = 128.0 + 127.0 * (frame as f64 / 60.0).sin();
        node.send_with("frames", FRAME_LEN, |buf| {
            buf[..HEADER_LEN].copy_from_slice(&frame.to_le_bytes());
            buf[HEADER_LEN..].fill(level as u8);
        })?;
        if frame % u64::from(FPS * 5) == 0 {
            println!("frame {frame}, brightness {level:.0}");
        }
        next += period;
        std::thread::sleep(next.saturating_duration_since(Instant::now()));
    }
    Ok(())
}
