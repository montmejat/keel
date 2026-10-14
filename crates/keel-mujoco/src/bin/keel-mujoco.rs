//! A simulated robot in MuJoCo, as one node: takes `command`, publishes
//! `state`, and an output per sensor and per camera, named as in its robot
//! file (see [`keel_mujoco::Robot`]). The same loops as a robot's own nodes
//! (`keel-can`'s, a sensor's, a camera's) run here over simulated devices,
//! each at its rate, on the simulation's clock.
//!
//! `--cycle same` (the default) runs in real time, closing each joints' tick
//! within the cycle; `next` too, a tick behind; `lockstep` steps as soon as
//! the controller has answered, as fast as the machine allows.
//!
//! `--break <device>:<fault>[@<seconds>]` breaks a part, several separated
//! by commas: `joint2:dead@10` (its drive gives no torque), `joint3:freeze`
//! (its encoder), `imu:noisy@5`, `wrist:dark@8` (see
//! [`keel_control::model::Breakdown`]). `KEEL_BREAK` in the environment does
//! the same, for a fleet's machines.
//!
//! `--loop drive|keel` and `--hz` override the robot file's: where position
//! loops close, and how often the joints' loop runs.
//!
//! `--robot examples/mujoco/piper.yml --cycle same --deadline-us 2000 --break '' --loop '' --hz 0`

use std::io;
use std::path::PathBuf;
use std::time::Duration;

use keel::Node;
use keel_control::cycle::Cycle;
use keel_control::schedule::{self, JointSettings};
use keel_control::sim::SimClock;
use keel_control::{arg, cycle};
use keel_mujoco::{Robot, Simulation};

fn main() -> io::Result<()> {
    let path: PathBuf = arg("robot", PathBuf::from("examples/mujoco/piper.yml"))?;
    let mut robot = Robot::read(&path)?;
    let breaks = arg("break", std::env::var("KEEL_BREAK").unwrap_or_default())?;
    // Silently: hardware doesn't say when it breaks.
    for part in breaks.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        robot.break_device(part)?;
    }
    let loop_at: String = arg("loop", String::new())?;
    if !loop_at.is_empty() {
        robot.joints.r#loop = loop_at.parse()?;
    }
    let hz: f64 = arg("hz", 0.0)?;
    if hz > 0.0 {
        robot.joints.hz = hz;
    }
    let joints = JointSettings {
        cycle: cycle::cycle_arg()?,
        deadline: Duration::from_micros(arg("deadline-us", 2000)?),
        ..JointSettings::default()
    };
    let mut node = Node::from_env()?;
    let sim = Simulation::build(&robot, joints, &node)?;
    for output in &sim.outputs {
        println!("publishing {output}");
    }
    let mut clock = SimClock::new(&sim.world, joints.cycle != Cycle::Lockstep);
    schedule::run(&mut node, &mut clock, sim.loops)
}
