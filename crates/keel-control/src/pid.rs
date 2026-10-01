//! A PID position controller for one joint.

use crate::State;

#[derive(Debug, Clone, Copy)]
pub struct Pid {
    pub kp: f64,
    pub ki: f64,
    pub kd: f64,
    /// The effort is kept within ± this.
    pub limit: f64,
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
        let effort = self.kp * error + self.ki * self.integral - self.kd * state.velocity;
        // Don't wind up while the limit is what's holding the joint back.
        if effort.abs() < self.limit || effort * error < 0.0 {
            self.integral += error * dt;
        }
        effort.clamp(-self.limit, self.limit)
    }
}
