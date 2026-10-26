//! A controller for Pollen's Microduck, behind `keel-dynamixel`: it holds
//! the standing pose its policies start from, and once a second says where
//! the joints are and which way is down, from the IMU.
//!
//! Positions only: the servos close their own loop (decision 0034).

use std::io;

use keel::{Event, Node};
use keel_control::{Command, Imu, State};

/// The standing pose, in the joints' order (`keel-dynamixel --motors`):
/// the left leg (hip yaw, roll, pitch, knee, ankle), the neck and head
/// (neck pitch, head pitch, yaw, roll, the mouth), the right leg. Pollen's
/// default pose (`duck-control`'s `model.rs`, and the policies' metadata).
const STAND: [f64; 15] = [
    0.0, -0.0873, -0.4579, -0.0049, 0.4530, //
    0.3491, 0.3491, 0.0, 0.0, 0.0, //
    0.0, 0.0873, 0.4579, 0.0049, -0.4530,
];

fn main() -> io::Result<()> {
    let mut node = Node::from_env()?;
    let (mut started, mut said, mut down) = (None, 0, None);
    let commands: Vec<Command> = STAND.iter().map(|&position| Command { position, ..Default::default() }).collect();
    while let Event::Input { id, data } = node.next_event()? {
        if id == "imu" {
            down = Imu::read(&data).map(|imu| imu.gravity());
            continue;
        }
        let now = data.stamp_ns();
        let t = (now - *started.get_or_insert(now)) as f64 / 1e9;
        if now.saturating_sub(said) >= 1_000_000_000 {
            said = now;
            let states: Vec<State> = State::read(&data).collect();
            let worst = states.iter().zip(STAND).map(|(s, p)| (s.position - p).abs()).fold(0.0, f64::max);
            let down = down.map_or("no IMU".into(), |g| format!("down [{:+.2} {:+.2} {:+.2}]", g[0], g[1], g[2]));
            println!("{t:4.1}s joints within {worst:.3} rad of standing; {down}");
        }
        // Sent while `data` is held: the command answers this state.
        node.send_with("command", commands.len() * Command::LEN, |p| Command::write(&commands, p))?;
    }
    Ok(())
}
