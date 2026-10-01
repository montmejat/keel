//! What's simulated: a pendulum driven at its pivot. Position 0 hangs
//! straight down, so holding any other angle takes a constant torque.

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
