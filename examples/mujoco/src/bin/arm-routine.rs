//! A pick-and-place routine for the Piper, in joint space: takes `state`,
//! sends `command`, a position for each of its seven joints (six, then the
//! gripper's opening in metres). Whoever closes the position loops, the
//! drives or keel, it doesn't know.
//!
//! It moves the red cube from one spot on the table to another, and back on
//! the next cycle: above it, down, grip, up, across, down, let go, up, home.
//! Between two poses the targets follow a smooth (cosine) path, reached in
//! `--move-s` and held for `--hold-s`. Its time comes from the states'
//! stamps, so it runs the same in lockstep as in real time.
//!
//! Each command answers the state it was computed from, which is what lets
//! the joints' loop close within the cycle (see `keel_control::cycle`).
//!
//! `--move-s 1.5 --hold-s 0.5`

use std::f64::consts::PI;

use keel::{Event, Node};
use keel_control::{arg, Command, State};

const OPEN: f64 = 0.035;
const SHUT: f64 = 0.0;

/// The first five joints over each spot and down at it, gripper pointing
/// down (solved for the scene's table and cube; the sixth turns the gripper
/// square to the cube).
const SPOTS: [([f64; 5], [f64; 5]); 2] = [
    ([0.376, 1.645, -1.320, 0.0, 1.220], [0.377, 2.075, -1.382, 0.0, 1.220]),
    ([-0.592, 1.756, -1.457, 0.0, 1.220], [-0.591, 2.124, -1.508, 0.0, 1.220]),
];
const HOME: [f64; 7] = [0.0, 1.57, -1.3485, 0.0, 0.0, 0.0, 0.0];

fn pose(arm: [f64; 5], gripper: f64) -> [f64; 7] {
    [arm[0], arm[1], arm[2], arm[3], arm[4], -arm[0], gripper]
}

/// A cycle's poses, from spot `from` to spot `to`.
fn cycle(from: usize, to: usize) -> [(&'static str, [f64; 7]); 9] {
    let ((above_a, at_a), (above_b, at_b)) = (SPOTS[from], SPOTS[to]);
    [
        ("above the cube", pose(above_a, OPEN)),
        ("down at it", pose(at_a, OPEN)),
        ("gripping", pose(at_a, SHUT)),
        ("lifting", pose(above_a, SHUT)),
        ("across", pose(above_b, SHUT)),
        ("down", pose(at_b, SHUT)),
        ("letting go", pose(at_b, OPEN)),
        ("up", pose(above_b, OPEN)),
        ("home", HOME),
    ]
}

fn main() -> std::io::Result<()> {
    let move_s: f64 = arg("move-s", 1.5)?;
    let hold_s: f64 = arg("hold-s", 0.5)?;
    let mut node = Node::from_env()?;
    let leg = move_s + hold_s;
    let mut start: Option<(u64, [f64; 7])> = None;
    let mut said = usize::MAX;
    let mut commands = [Command::default(); 7];

    while let Event::Input { data, .. } = node.next_event()? {
        let joints: Vec<State> = State::read(&data).collect();
        if joints.len() != 7 {
            return Err(std::io::Error::other(format!("a Piper has 7 joints, not {}", joints.len())));
        }
        // The routine starts from wherever the arm is.
        let (t0, from) = *start.get_or_insert_with(|| {
            let mut at = [0.0; 7];
            at.iter_mut().zip(&joints).for_each(|(a, s)| *a = s.position);
            (data.stamp_ns(), at)
        });
        let t = (data.stamp_ns() - t0) as f64 / 1e9;
        let step = (t / leg) as usize;
        // Even cycles carry the cube from the first spot to the second; odd
        // ones bring it back.
        let n = cycle(0, 1).len();
        let (round, i) = (step / n, step % n);
        let poses = if round % 2 == 0 { cycle(0, 1) } else { cycle(1, 0) };
        if step != said {
            if i == 0 && step > 0 {
                println!("cycle {round} done at t {t:.1} s");
            }
            println!("{}", poses[i].0);
            said = step;
        }
        let previous = if step == 0 {
            from
        } else if i == 0 {
            HOME
        } else {
            poses[i - 1].1
        };
        let along = ((t % leg) / move_s).min(1.0);
        let s = (1.0 - (along * PI).cos()) / 2.0;
        for ((c, a), b) in commands.iter_mut().zip(previous).zip(poses[i].1) {
            *c = Command::position(a + (b - a) * s);
        }
        node.send_with("command", commands.len() * Command::LEN, |payload| Command::write(&commands, payload))?;
    }
    Ok(())
}
