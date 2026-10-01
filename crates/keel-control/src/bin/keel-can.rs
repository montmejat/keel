//! Joints on a CAN bus: sends each `command` as frames, and publishes the
//! `state` the drives report. A cyclic bus master: once per period it takes
//! what arrived on both sides and passes it on, so it adds up to a period of
//! delay each way.
//!
//! `--interface can0 --joints 1 --hz 1000`

use std::io;
use std::time::{Duration, Instant};

use keel::{Event, Node, Periodic};
use keel_control::can::{self, Bus};
use keel_control::{arg, Command, State};

/// Drives silent for this long get a line in the log.
const SILENCE: Duration = Duration::from_secs(1);

fn main() -> io::Result<()> {
    let interface: String = arg("interface", "can0".into())?;
    let joints: usize = arg("joints", 1)?;
    if joints > can::MAX_JOINTS {
        return Err(io::Error::other(format!("a bus carries at most {} joints", can::MAX_JOINTS)));
    }
    let period = Duration::from_secs_f64(1.0 / arg("hz", 1000.0)?);
    let bus = Bus::open(&interface)?;
    let mut node = Node::from_env()?;
    let mut states = vec![State::default(); joints];
    // Which joints have reported since the last `state`.
    let mut fresh = vec![false; joints];
    let (mut heard, mut complained) = (Instant::now(), false);

    let mut periodic = Periodic::new(period);
    loop {
        periodic.wait();
        while let Some(event) = node.try_next_event()? {
            match event {
                Event::Input { data, .. } => {
                    for (joint, command) in Command::read(&data).take(joints).enumerate() {
                        bus.send(&can::command_frame(joint, command))?;
                    }
                }
                Event::Stop => return Ok(()),
            }
        }
        while let Some(frame) = bus.try_recv()? {
            if let Some((joint, state)) = can::as_state(&frame, joints) {
                (states[joint], fresh[joint]) = (state, true);
            }
        }
        // A state is every joint's: nothing is published from half a bus.
        if fresh.iter().all(|&f| f) {
            fresh.fill(false);
            if complained {
                println!("`{interface}` is back");
            }
            (heard, complained) = (Instant::now(), false);
            node.send_with("state", joints * State::LEN, |payload| State::write(&states, payload))?;
        } else if !complained && heard.elapsed() >= SILENCE {
            complained = true;
            println!("no state from every joint on `{interface}` for {SILENCE:?}");
        }
    }
}
