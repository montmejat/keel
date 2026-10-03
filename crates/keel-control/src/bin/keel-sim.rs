//! Simulated joints: pendulums stepped in real time. Takes `command`,
//! publishes `state`, like `keel-can` does for real ones.
//!
//! Each tick publishes the state, waits up to `--deadline-us` for the command
//! that answers it, and steps with it: the loop closes within the cycle (see
//! [`keel_control::cycle`]). A late command waits for the next tick, and the
//! joints step with the previous one meanwhile. `--cycle next` steps with
//! whatever arrived before the tick instead: a constant delay of one period.
//!
//! `--joints 1 --hz 1000 --cycle same --deadline-us 300 --spin-us 0`

use std::io;
use std::time::Duration;

use keel::{Event, Node, Periodic};
use keel_control::cycle::{self, Answer, Misses};
use keel_control::plant::Pendulum;
use keel_control::{arg, Command, State};

fn main() -> io::Result<()> {
    let joints: usize = arg("joints", 1)?;
    let dt = 1.0 / arg("hz", 1000.0)?;
    let same_cycle = cycle::same_cycle_arg()?;
    let deadline = Duration::from_micros(arg("deadline-us", 300)?);
    let mut node = Node::from_env()?;
    node.set_spin(Duration::from_micros(arg("spin-us", 0)?));
    // Nothing to wait for without a controller.
    let same_cycle = same_cycle && !node.inputs().is_empty();
    let mut plants = vec![Pendulum::default(); joints];
    let mut commands = vec![Command::default(); joints];
    let mut states = vec![State::default(); joints];
    let mut misses = Misses::new(deadline);

    let mut periodic = Periodic::new(Duration::from_secs_f64(dt));
    loop {
        periodic.wait();
        let mut take = |data: &[u8]| {
            commands.iter_mut().zip(Command::read(data)).for_each(|(c, new)| *c = new);
            Ok(())
        };
        if same_cycle {
            states.iter_mut().zip(&plants).for_each(|(state, plant)| *state = plant.state());
            node.send_with("state", joints * State::LEN, |payload| State::write(&states, payload))?;
            let asked = node.last_sent_span();
            let answer = cycle::wait_for_answer(&mut node, asked, deadline, &mut take)?;
            if answer == Answer::Stop {
                return Ok(());
            }
            misses.count(answer);
            plants.iter_mut().zip(&commands).for_each(|(plant, command)| plant.step(command.effort, dt));
        } else {
            while let Some(event) = node.try_next_event()? {
                match event {
                    Event::Input { data, .. } => take(&data)?,
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
}
