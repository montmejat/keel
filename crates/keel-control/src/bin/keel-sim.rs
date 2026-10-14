//! Simulated joints: pendulums. Takes `command`, publishes `state`, like
//! `keel-can` does for real ones, with the same loop
//! ([`keel_control::schedule::JointLoop`]) over simulated joints instead of
//! a bus. Each state is stamped with the simulation's time, which starts at
//! the clock's and advances a step at a time.
//!
//! Each tick publishes the state, waits up to `--deadline-us` for the command
//! that answers it, and steps with it: the loop closes within the cycle (see
//! [`keel_control::cycle`]). A late command waits for the next tick, and the
//! joints step with the previous one meanwhile. `--cycle next` steps with
//! whatever arrived before the tick instead: a constant delay of one period.
//! `--cycle lockstep` has no clock: it steps as soon as the controller has
//! answered, faster or slower than real time.
//!
//! Commands may be efforts, or positions and velocities: the simulated
//! drives close those loops (`--loop drive`), or the joints' loop does
//! (`--loop keel`, with `--kp --ki --kd --limit`).
//!
//! `--joints 1 --hz 1000 --cycle same --deadline-us 300 --loop drive --spin-us 0`

use std::io;
use std::time::Duration;

use keel::Node;
use keel_control::drive::DriveSettings;
use keel_control::model::VectorSettings;
use keel_control::schedule::{self, JointLoop, JointSettings};
use keel_control::sim::{Pendulums, SimClock, SimJoints, World};
use keel_control::{arg, cycle::Cycle};

fn main() -> io::Result<()> {
    let joints: usize = arg("joints", 1)?;
    let settings = JointSettings::from_args(JointSettings::default())?;
    let mut node = Node::from_env()?;
    node.set_spin(Duration::from_micros(arg("spin-us", 0)?));

    // A physics step per tick.
    let timestep = (1e9 / settings.hz) as u64;
    let world = World::new(Box::new(Pendulums::new(joints, timestep)), |_| DriveSettings::default())?;
    let device = SimJoints::new(&world, |_| VectorSettings::default())?;
    let joints = JointLoop::new(device, settings, &node)?;
    let mut clock = SimClock::new(&world, settings.cycle != Cycle::Lockstep);
    schedule::run(&mut node, &mut clock, vec![Box::new(joints)])
}
