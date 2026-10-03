//! Holds every joint at a target position: a `command` for each `state`.
//! It doesn't know what's behind them.
//!
//! `--spin-us` watches for states for that long before sleeping (see
//! `Node::set_spin`): on a CPU of its own, a faster answer.
//!
//! `--target 1.0 --kp 40 --ki 40 --kd 4 --limit 10 --spin-us 0`

use std::io;
use std::time::{Duration, Instant};

use keel::{Event, Node};
use keel_control::pid::Pid;
use keel_control::{arg, Command, State};

/// A gap between states longer than this isn't integrated over: the joint
/// was away, not slow.
const MAX_STEP: f64 = 0.01;

fn main() -> io::Result<()> {
    let target: f64 = arg("target", 1.0)?;
    let default = Pid::default();
    let pid =
        Pid::new(arg("kp", default.kp)?, arg("ki", default.ki)?, arg("kd", default.kd)?, arg("limit", default.limit)?);
    let mut node = Node::from_env()?;
    node.set_spin(Duration::from_micros(arg("spin-us", 0)?));
    let mut pids: Vec<Pid> = Vec::new();
    let mut commands: Vec<Command> = Vec::new();

    let started = Instant::now();
    let (mut last, mut said) = (started, started);
    while let Event::Input { data, .. } = node.next_event()? {
        let now = Instant::now();
        let dt = now.duration_since(last).as_secs_f64().min(MAX_STEP);
        last = now;
        let joints = data.len() / State::LEN;
        pids.resize(joints, pid);
        commands.resize(joints, Command::default());
        for ((state, pid), command) in State::read(&data).zip(&mut pids).zip(&mut commands) {
            command.effort = pid.update(target, state, dt);
        }
        if now.duration_since(said) >= Duration::from_secs(1) {
            said = now;
            let joints: Vec<String> = (State::read(&data).zip(&commands))
                .map(|(s, c)| {
                    format!("at {:.3} rad, {:+.3} rad/s, pushing {:+.2} N m", s.position, s.velocity, c.effort)
                })
                .collect();
            println!("{:.0}s target {target}: {}", now.duration_since(started).as_secs_f64(), joints.join("; "));
        }
        // Sent while `data` is held: the command belongs to the state's trace.
        node.send_with("command", joints * Command::LEN, |payload| Command::write(&commands, payload))?;
    }
    Ok(())
}
