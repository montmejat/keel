//! A pretend microcontroller driving one joint, to try `keel-micro` without
//! hardware: its "firmware" loop is what a chip would run, over a
//! pseudo-terminal instead of a UART, with `keel-sim`'s pendulum for a
//! motor and its drive model for the firmware's motor control. It's a node only so that keel starts and stops it with the rest.
//!
//! `--port /tmp/keel-chip0 --hz 1000`

use std::io;
use std::path::PathBuf;
use std::time::Duration;

use keel::trace::now_ns;
use keel::{Event, Node, Periodic};
use keel_control::drive::{Drive, DriveSettings};
use keel_control::plant::Pendulum;
use keel_control::{arg, Command, State};
use keel_serial::Port;

/// Channels, as the chip and `keel-serial`'s `--outputs` / `--inputs` agree.
const STATE: u8 = 0;
const COMMAND: u8 = 0;

fn main() -> io::Result<()> {
    let device: PathBuf = arg("port", "/tmp/keel-chip0".into())?;
    let dt = 1.0 / arg("hz", 1000.0)?;
    let mut chip: keel_micro::Node<Port, 64> = keel_micro::Node::new(Port::pretend(&device)?);
    let mut node = Node::from_env()?;
    let mut motor = Pendulum::default();
    // Lets the motor go without a command for 100 ms.
    let mut drive = Drive::new(DriveSettings::default())?;

    let mut periodic = Periodic::new(Duration::from_secs_f64(dt));
    loop {
        periodic.wait();
        if let Some(Event::Stop) = node.try_next_event()? {
            let _ = std::fs::remove_file(&device);
            return Ok(());
        }
        // The firmware, from here on.
        while let Some((COMMAND, payload)) = chip.poll() {
            if let Some(command) = Command::read(payload).next() {
                drive.command(now_ns(), command);
            }
        }
        motor.step(drive.torque(now_ns(), motor.state(), dt), dt);
        let mut state = [0; State::LEN];
        State::write(&[motor.state()], &mut state);
        chip.send(STATE, &state);
    }
}
