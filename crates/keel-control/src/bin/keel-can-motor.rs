//! Stands in for drives on a CAN bus: the pendulums of `keel-sim`, reached
//! only through frames, each behind the same drive model as a simulated
//! arm's ([`keel_control::drive`]). A node with no inputs or outputs, there
//! so that keel starts and stops it with the rest.
//!
//! Like a real drive, it applies a command when its frame arrives, closing
//! the loop itself for a position or a velocity, and lets go when commands
//! stop coming. It reports each joint's state once per period, on its own
//! tick: give it a `phase_us` that puts the report just before the bus
//! master's tick, or the master may read the bus first.
//!
//! `--interface vcan0 --joints 1 --hz 1000`

use std::io;
use std::time::Duration;

use keel::periodic::{self, next_tick};
use keel::trace::now_ns;
use keel::{Event, Node};
use keel_control::arg;
use keel_control::can::{self, Bus};
use keel_control::drive::{Drive, DriveSettings};
use keel_control::plant::Pendulum;

fn main() -> io::Result<()> {
    let interface: String = arg("interface", "vcan0".into())?;
    let joints: usize = arg("joints", 1)?;
    let period = (1e9 / arg("hz", 1000.0)?) as u64;
    let bus = Bus::open(&interface)?;
    let mut node = Node::from_env()?;
    let mut drives = Drives {
        plants: vec![Pendulum::default(); joints],
        drives: (0..joints).map(|_| Drive::new(DriveSettings::default())).collect::<io::Result<_>>()?,
        stepped: now_ns(),
    };

    let mut tick = next_tick(now_ns(), period, periodic::phase().as_nanos() as u64);
    loop {
        // Until the tick, commands take effect as they arrive.
        loop {
            let now = now_ns();
            if now >= tick {
                break;
            }
            if bus.wait(Duration::from_nanos(tick - now))? {
                let now = now_ns();
                while let Some(frame) = bus.try_recv()? {
                    if let Some((joint, command)) = can::as_command(&frame, joints) {
                        drives.step_to(now);
                        drives.drives[joint].command(now, command);
                    }
                }
            }
        }
        drives.step_to(tick);
        for (joint, plant) in drives.plants.iter().enumerate() {
            bus.send(&can::state_frame(joint, plant.state()))?;
        }
        if let Some(Event::Stop) = node.try_next_event()? {
            return Ok(());
        }
        // Ticks missed while busy are skipped, not caught up.
        tick = next_tick(now_ns().max(tick), period, tick % period);
    }
}

struct Drives {
    plants: Vec<Pendulum>,
    drives: Vec<Drive>,
    /// How far the pendulums have been stepped.
    stepped: u64,
}

impl Drives {
    /// Steps every pendulum to `t` under its drive's torque.
    fn step_to(&mut self, t: u64) {
        let dt = t.saturating_sub(self.stepped) as f64 / 1e9;
        if dt <= 0.0 {
            return;
        }
        for (plant, drive) in self.plants.iter_mut().zip(&mut self.drives) {
            plant.step(drive.torque(t, plant.state(), dt), dt);
        }
        self.stepped = t;
    }
}
