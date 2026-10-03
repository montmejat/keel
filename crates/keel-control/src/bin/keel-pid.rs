//! Holds every joint at a target position: a `command` for each `state`.
//! It doesn't know what's behind them, nor whether time is real: its time
//! steps are the gaps between the states' stamps, so it behaves the same
//! against joints, a simulation in lockstep, or a replay at any speed. The
//! log line comes every second of that time.
//!
//! `--spin-us` watches for states for that long before sleeping (see
//! `Node::set_spin`): on a CPU of its own, a faster answer. With
//! `--period-us`, the period states come at, it watches only around when
//! they're due (`Node::set_spin_around`; the node's `phase_us` says when).
//!
//! `--target 1.0 --kp 40 --ki 40 --kd 4 --limit 10 --spin-us 0 --period-us 0`

use std::io;
use std::time::Duration;

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
    let spin = Duration::from_micros(arg("spin-us", 0)?);
    match arg("period-us", 0)? {
        0 => node.set_spin(spin),
        period => node.set_spin_around(spin, Duration::from_micros(period)),
    }
    let mut pids: Vec<Pid> = Vec::new();
    let mut commands: Vec<Command> = Vec::new();

    let (mut started, mut last, mut said) = (None, 0, 0);
    while let Event::Input { data, .. } = node.next_event()? {
        let now = data.stamp_ns();
        let started = *started.get_or_insert_with(|| {
            said = now;
            now
        });
        let dt = if last == 0 { 0.0 } else { (now.saturating_sub(last) as f64 / 1e9).min(MAX_STEP) };
        last = now;
        let joints = data.len() / State::LEN;
        pids.resize(joints, pid);
        commands.resize(joints, Command::default());
        for ((state, pid), command) in State::read(&data).zip(&mut pids).zip(&mut commands) {
            command.effort = pid.update(target, state, dt);
        }
        if now.saturating_sub(said) >= 1_000_000_000 {
            said = now;
            let joints: Vec<String> = (State::read(&data).zip(&commands))
                .map(|(s, c)| {
                    format!("at {:.3} rad, {:+.3} rad/s, pushing {:+.2} N m", s.position, s.velocity, c.effort)
                })
                .collect();
            println!("{:.0}s target {target}: {}", (now - started) as f64 / 1e9, joints.join("; "));
        }
        // Sent while `data` is held: the command belongs to the state's trace.
        node.send_with("command", joints * Command::LEN, |payload| Command::write(&commands, payload))?;
    }
    Ok(())
}
