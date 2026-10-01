//! Simulated joints: pendulums stepped in real time. Takes `command`,
//! publishes `state`, like `keel-can` does for real ones.
//!
//! `--joints 1 --hz 1000`

use std::io;
use std::time::Duration;

use keel::{Event, Node, Periodic};
use keel_control::plant::Pendulum;
use keel_control::{arg, Command, State};

fn main() -> io::Result<()> {
    let joints: usize = arg("joints", 1)?;
    let dt = 1.0 / arg("hz", 1000.0)?;
    let mut node = Node::from_env()?;
    let mut plants = vec![Pendulum::default(); joints];
    let mut commands = vec![Command::default(); joints];
    let mut states = vec![State::default(); joints];

    let mut periodic = Periodic::new(Duration::from_secs_f64(dt));
    loop {
        periodic.wait();
        while let Some(event) = node.try_next_event()? {
            match event {
                Event::Input { data, .. } => {
                    commands.iter_mut().zip(Command::read(&data)).for_each(|(c, new)| *c = new)
                }
                Event::Stop => return Ok(()),
            }
        }
        for ((plant, command), state) in plants.iter_mut().zip(&commands).zip(&mut states) {
            plant.step(command.effort, dt);
            *state = plant.state();
        }
        node.send_with("state", joints * State::LEN, |payload| State::write(&states, payload))?;
    }
}
