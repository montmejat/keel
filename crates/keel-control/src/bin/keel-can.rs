//! Joints on a CAN bus: sends each `command` as frames, and publishes the
//! `state` the drives report. A cyclic bus master, the joints' loop
//! ([`keel_control::schedule::JointLoop`]) over the bus
//! ([`keel_control::can::CanJoints`]): once per period it publishes what the
//! drives reported, stamped with when it read them, waits up to
//! `--deadline-us` for the command that answers it, and sends that straight
//! away (see [`keel_control::cycle`]). A command that misses the deadline
//! goes out at the next tick, and the drives keep the previous one
//! meanwhile. `--cycle next` sends every command at the next tick: up to a
//! period of delay.
//!
//! Position and velocity commands go to the drives, which close the loop
//! (`--loop drive`), or are turned into efforts here (`--loop keel`, with
//! `--kp --ki --kd --limit`).
//!
//! `--interface can0 --joints 1 --hz 1000 --cycle same --deadline-us 300 --loop drive --spin-us 0`

use std::io;
use std::time::Duration;

use keel::Node;
use keel_control::arg;
use keel_control::can::CanJoints;
use keel_control::cycle::Cycle;
use keel_control::schedule::{self, JointLoop, JointSettings, Wall};

fn main() -> io::Result<()> {
    let interface: String = arg("interface", "can0".into())?;
    let joints: usize = arg("joints", 1)?;
    let settings = JointSettings::from_args(JointSettings::default())?;
    if settings.cycle == Cycle::Lockstep {
        return Err(io::Error::other("--cycle lockstep is for simulations: a bus keeps time"));
    }
    let bus = CanJoints::open(&interface, joints)?;
    let mut node = Node::from_env()?;
    node.set_spin(Duration::from_micros(arg("spin-us", 0)?));
    let joints = JointLoop::new(bus, settings, &node)?;
    schedule::run(&mut node, &mut Wall, vec![Box::new(joints)])
}
