//! A controller for OpenArm v1's seven joints and its gripper, behind
//! `keel-can --protocol damiao`: holds the arm at its home pose with
//! openarm_ros2's gains, swings the first joint ±0.4 rad at a quarter hertz,
//! and opens and closes the gripper every two seconds.
//!
//! The loop closes on the motors: each command is a target and two gains
//! (MIT mode), not a torque. Time is the states' stamps, so it runs the same
//! against the arm, drydock, or a replay. Once a second (of that time), where
//! the joints are.

use std::f64::consts::TAU;
use std::io;

use keel::{Event, Node};
use keel_control::{Command, State};

/// openarm_ros2's gains: seven joints, then the gripper's motor.
const KP: [f64; 8] = [70.0, 70.0, 70.0, 60.0, 10.0, 10.0, 10.0, 5.0];
const KD: [f64; 8] = [2.75, 2.5, 2.0, 2.0, 0.7, 0.6, 0.5, 0.1];
/// The elbow (joint 4) bent a quarter turn; the gripper open.
const HOME: [f64; 8] = [0.0, 0.0, 0.0, std::f64::consts::FRAC_PI_2, 0.0, 0.0, 0.0, 0.0];
/// The gripper motor's angle with the fingers closed: a sixth of a turn
/// (openarm_ros2's -1.0472).
const CLOSED: f64 = -std::f64::consts::FRAC_PI_3;
const SWING: f64 = 0.4;
const SWING_HZ: f64 = 0.25;

fn main() -> io::Result<()> {
    let mut node = Node::from_env()?;
    let (mut started, mut said) = (None, 0);
    let mut commands = Vec::new();
    while let Event::Input { data, .. } = node.next_event()? {
        let now = data.stamp_ns();
        let t = (now - *started.get_or_insert(now)) as f64 / 1e9;
        let states: Vec<State> = State::read(&data).collect();
        commands.clear();
        for i in 0..states.len() {
            let (position, velocity) = match i {
                0 => (SWING * (TAU * SWING_HZ * t).sin(), SWING * TAU * SWING_HZ * (TAU * SWING_HZ * t).cos()),
                7 => (if t % 4.0 < 2.0 { 0.0 } else { CLOSED }, 0.0),
                _ => (HOME.get(i).copied().unwrap_or(0.0), 0.0),
            };
            let (kp, kd) = (KP.get(i).copied().unwrap_or(0.0), KD.get(i).copied().unwrap_or(0.0));
            commands.push(Command { position, velocity, kp, kd, effort: 0.0 });
        }
        if now.saturating_sub(said) >= 1_000_000_000 {
            said = now;
            let at: Vec<String> = states.iter().map(|s| format!("{:+.2}", s.position)).collect();
            println!("{t:4.1}s joints at {} rad", at.join(" "));
        }
        // Sent while `data` is held: the command answers this state, within
        // its cycle.
        node.send_with("command", commands.len() * Command::LEN, |p| Command::write(&commands, p))?;
    }
    Ok(())
}
