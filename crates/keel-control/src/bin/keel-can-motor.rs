//! Stands in for drives on a CAN bus: the pendulums of `keel-sim`, reached
//! only through frames. A node with no inputs or outputs, there so that keel
//! starts and stops it with the rest.
//!
//! Like a real drive, it lets go when commands stop coming.
//!
//! `--interface vcan0 --joints 1 --hz 1000`

use std::io;
use std::time::{Duration, Instant};

use keel::{Event, Node, Periodic};
use keel_control::can::{self, Bus};
use keel_control::plant::Pendulum;
use keel_control::{arg, Command};

/// Without a command for this long, a joint's effort drops to zero.
const COMMAND_TIMEOUT: Duration = Duration::from_millis(100);

fn main() -> io::Result<()> {
    let interface: String = arg("interface", "vcan0".into())?;
    let joints: usize = arg("joints", 1)?;
    let dt = 1.0 / arg("hz", 1000.0)?;
    let bus = Bus::open(&interface)?;
    let mut node = Node::from_env()?;
    let mut plants = vec![Pendulum::default(); joints];
    let mut commands = vec![(Command::default(), Instant::now()); joints];

    let mut periodic = Periodic::new(Duration::from_secs_f64(dt));
    loop {
        periodic.wait();
        if let Some(Event::Stop) = node.try_next_event()? {
            return Ok(());
        }
        let now = Instant::now();
        while let Some(frame) = bus.try_recv()? {
            if let Some((joint, command)) = can::as_command(&frame, joints) {
                commands[joint] = (command, now);
            }
        }
        for (joint, (plant, (command, at))) in plants.iter_mut().zip(&commands).enumerate() {
            let effort = if now.duration_since(*at) < COMMAND_TIMEOUT { command.effort } else { 0.0 };
            plant.step(effort, dt);
            bus.send(&can::state_frame(joint, plant.state()))?;
        }
    }
}
