//! Control laws: pure maths, from a target, a joint's state and the time
//! since the last call to an effort. No keel, no clock: the same law runs in
//! a controller node, in a joints node closing the loop itself, or in a
//! simulated drive (see [`crate::drive`]), and is tested on its own.

use crate::{Command, Mode, State};

/// A PID for one joint. Its gains serve both its position loop and its
/// velocity loop: a joint is in one mode at a time.
#[derive(Debug, Clone, Copy, PartialEq, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Pid {
    pub kp: f64,
    pub ki: f64,
    pub kd: f64,
    /// The effort is kept within ± this.
    pub limit: f64,
    #[serde(skip)]
    integral: f64,
}

impl Default for Pid {
    fn default() -> Self {
        Self::new(40.0, 40.0, 4.0, 10.0)
    }
}

impl Pid {
    pub fn new(kp: f64, ki: f64, kd: f64, limit: f64) -> Self {
        Self { kp, ki, kd, limit, integral: 0.0 }
    }

    /// The effort that moves the joint towards `target`, `dt` seconds after
    /// the previous call. The derivative is taken on the measured velocity,
    /// so a change of target doesn't kick.
    pub fn update(&mut self, target: f64, state: State, dt: f64) -> f64 {
        let error = target - state.position;
        self.integrate(error, self.kp * error - self.kd * state.velocity, dt)
    }

    /// The effort that brings the joint to `target` radians per second: a
    /// PI on the velocity.
    pub fn update_velocity(&mut self, target: f64, state: State, dt: f64) -> f64 {
        let error = target - state.velocity;
        self.integrate(error, self.kp * error, dt)
    }

    /// The effort a command asks for: through the loop its mode needs, or
    /// as it is.
    pub fn effort(&mut self, command: Command, state: State, dt: f64) -> f64 {
        match command.mode {
            Mode::Effort => command.value.clamp(-self.limit, self.limit),
            Mode::Position => self.update(command.value, state, dt),
            Mode::Velocity => self.update_velocity(command.value, state, dt),
        }
    }

    /// Adds the integral term to `effort`, limited. The integral doesn't wind
    /// up while the limit is what's holding the joint back.
    fn integrate(&mut self, error: f64, effort: f64, dt: f64) -> f64 {
        let effort = effort + self.ki * self.integral;
        if effort.abs() < self.limit || effort * error < 0.0 {
            self.integral += error * dt;
        }
        effort.clamp(-self.limit, self.limit)
    }

    /// Forgets the integral: the joint was let go, or changed mode.
    pub fn reset(&mut self) {
        self.integral = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plant::Pendulum;

    #[test]
    fn velocity_loop_spins_the_pendulum() {
        // Gravity speeds it up and slows it down along a turn: on average,
        // over the last two seconds, it goes as fast as asked.
        let (mut plant, mut pid, dt) = (Pendulum::default(), Pid::new(5.0, 20.0, 0.0, 10.0), 0.001);
        for _ in 0..5000 {
            plant.step(pid.update_velocity(2.0, plant.state(), dt), dt);
        }
        let from = plant.state().position;
        for _ in 0..2000 {
            plant.step(pid.update_velocity(2.0, plant.state(), dt), dt);
        }
        let mean = (plant.state().position - from) / 2.0;
        assert!((mean - 2.0).abs() < 0.1, "{mean} rad/s");
    }

    #[test]
    fn efforts_pass_through_within_the_limit() {
        let mut pid = Pid::default();
        assert_eq!(pid.effort(Command::effort(3.0), State::default(), 0.001), 3.0);
        assert_eq!(pid.effort(Command::effort(-30.0), State::default(), 0.001), -pid.limit);
    }
}
