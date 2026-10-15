//! What the tests drive: a pendulum per joint, driven at its pivot. Position
//! 0 hangs straight down, so holding any other angle takes a constant
//! torque. Only for the tests: the simulation is MuJoCo's (`keel-mujoco`).

use crate::sim::Physics;
use crate::State;

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

/// Pendulums as a [`Physics`]: no sensors, no cameras.
pub struct Pendulums {
    plants: Vec<Pendulum>,
    torques: Vec<f64>,
    time: u64,
    timestep: u64,
}

impl Pendulums {
    pub fn new(joints: usize, timestep_ns: u64) -> Pendulums {
        Pendulums {
            plants: vec![Pendulum::default(); joints],
            torques: vec![0.0; joints],
            time: 0,
            timestep: timestep_ns,
        }
    }
}

impl Physics for Pendulums {
    fn time_ns(&self) -> u64 {
        self.time
    }

    fn timestep_ns(&self) -> u64 {
        self.timestep
    }

    fn joints(&self) -> usize {
        self.plants.len()
    }

    fn joint(&self, i: usize) -> State {
        self.plants[i].state()
    }

    fn set_torque(&mut self, i: usize, torque: f64) {
        self.torques[i] = torque;
    }

    fn step(&mut self) {
        let dt = self.timestep as f64 / 1e9;
        self.plants.iter_mut().zip(&self.torques).for_each(|(p, &t)| p.step(t, dt));
        self.time += self.timestep;
    }
}
