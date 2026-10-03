//! Simulated joints: pendulums. Takes `command`, publishes `state`, like
//! `keel-can` does for real ones. Each state is stamped with the simulation's
//! time, which starts at the clock's and advances a step at a time.
//!
//! Each tick publishes the state, waits up to `--deadline-us` for the command
//! that answers it, and steps with it: the loop closes within the cycle (see
//! [`keel_control::cycle`]). A late command waits for the next tick, and the
//! joints step with the previous one meanwhile. `--cycle next` steps with
//! whatever arrived before the tick instead: a constant delay of one period.
//! `--cycle lockstep` has no clock: it steps as soon as the controller has
//! answered, faster or slower than real time.
//!
//! `--joints 1 --hz 1000 --cycle same --deadline-us 300 --spin-us 0`

use std::io;
use std::time::Duration;

use keel::trace::now_ns;
use keel::{Event, Node, Periodic};
use keel_control::cycle::{self, Answer, Cycle, Misses};
use keel_control::plant::Pendulum;
use keel_control::{arg, Command, State};

fn main() -> io::Result<()> {
    let joints: usize = arg("joints", 1)?;
    let dt = 1.0 / arg("hz", 1000.0)?;
    let cycle = cycle::cycle_arg()?;
    let deadline = Duration::from_micros(arg("deadline-us", 300)?);
    let mut node = Node::from_env()?;
    node.set_spin(Duration::from_micros(arg("spin-us", 0)?));
    let controlled = !node.inputs().is_empty();
    if cycle == Cycle::Lockstep && !controlled {
        return Err(io::Error::other("--cycle lockstep steps when the controller answers: it needs a `command` input"));
    }
    // Nothing to wait for without a controller.
    let cycle = if controlled { cycle } else { Cycle::Next };
    let mut plants = vec![Pendulum::default(); joints];
    let mut commands = vec![Command::default(); joints];
    let mut states = vec![State::default(); joints];
    let mut misses = Misses::new(deadline);

    let period = Duration::from_secs_f64(dt);
    let mut periodic = Periodic::new(period);
    let mut sim_ns = now_ns();
    loop {
        if cycle != Cycle::Lockstep {
            periodic.wait();
        }
        let mut take = |data: &[u8]| {
            commands.iter_mut().zip(Command::read(data)).for_each(|(c, new)| *c = new);
            Ok(())
        };
        if cycle == Cycle::Next {
            while let Some(event) = node.try_next_event()? {
                match event {
                    Event::Input { data, .. } => take(&data)?,
                    Event::Stop => return Ok(()),
                }
            }
        }
        states.iter_mut().zip(&plants).for_each(|(state, plant)| *state = plant.state());
        node.send_stamped("state", sim_ns, joints * State::LEN, |payload| State::write(&states, payload))?;
        if cycle != Cycle::Next {
            let asked = node.last_sent_span();
            let timeout = (cycle == Cycle::Same).then_some(deadline);
            let answer = cycle::wait_for_answer(&mut node, asked, timeout, &mut take)?;
            if answer == Answer::Stop {
                return Ok(());
            }
            misses.count(answer);
        }
        plants.iter_mut().zip(&commands).for_each(|(plant, command)| plant.step(command.effort, dt));
        sim_ns += period.as_nanos() as u64;
    }
}
