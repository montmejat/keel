//! What's simulated: a pendulum driven at its pivot. Position 0 hangs
//! straight down, so holding any other angle takes a constant torque.

use std::io;

use keel::trace::now_ns;

use crate::joints::Joints;
use crate::{Command, State};

const MASS: f64 = 1.0; // kg, at the tip
const LENGTH: f64 = 0.5; // m
const DAMPING: f64 = 0.1; // N m s/rad
const GRAVITY: f64 = 9.81; // m/s²

#[derive(Debug, Clone, Copy, Default)]
pub struct Pendulum {
    state: State,
}

impl Pendulum {
    pub fn state(&self) -> State {
        self.state
    }

    /// Advances by `dt` seconds under `effort` N m. Semi-implicit Euler:
    /// stable at the step sizes a control loop runs at.
    pub fn step(&mut self, effort: f64, dt: f64) {
        let State { position, velocity } = &mut self.state;
        let torque = effort - DAMPING * *velocity - MASS * GRAVITY * LENGTH * position.sin();
        *velocity += torque / (MASS * LENGTH * LENGTH) * dt;
        *position += *velocity * dt;
    }
}

/// Simulated joints: pendulums, stepped a period at a time by the drives'
/// law on each command. Their states are stamped with the simulation's time,
/// which starts at the clock's and advances a step per cycle, so a
/// controller can't tell real time from lockstep.
pub struct Pendulums {
    plants: Vec<Pendulum>,
    dt: f64,
    sim_ns: u64,
}

impl Pendulums {
    pub fn new(joints: usize, hz: f64) -> Self {
        Self { plants: vec![Pendulum::default(); joints], dt: 1.0 / hz, sim_ns: now_ns() }
    }
}

impl Joints for Pendulums {
    fn count(&self) -> usize {
        self.plants.len()
    }

    fn read(&mut self, states: &mut [State]) -> io::Result<Option<u64>> {
        states.iter_mut().zip(&self.plants).for_each(|(s, p)| *s = p.state());
        Ok(Some(self.sim_ns))
    }

    fn write(&mut self, commands: Option<&[Command]>, _states: &[State]) -> io::Result<()> {
        for (i, plant) in self.plants.iter_mut().enumerate() {
            let torque = commands.map_or(0.0, |c| c[i].torque(plant.state()));
            plant.step(torque, self.dt);
        }
        self.sim_ns += (self.dt * 1e9) as u64;
        Ok(())
    }
}
