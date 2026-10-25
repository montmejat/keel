//! Simulated joints: pendulums ([`keel_control::plant::Pendulums`]), run by
//! the joints' loop like `keel-can`'s real ones. Each state is stamped with
//! the simulation's time, which starts at the clock's and advances a step at
//! a time.
//!
//! Each tick publishes the state, waits up to `--deadline-us` for the command
//! that answers it, and steps with it: the loop closes within the cycle (see
//! [`keel_control::cycle`]). A late command waits for the next tick, and the
//! joints step with the previous one meanwhile. `--cycle next` steps with
//! whatever arrived before the tick instead: a constant delay of one period.
//! `--cycle lockstep` has no clock: it steps as soon as the controller has
//! answered, faster or slower than real time.
//!
//! `--joints 1 --hz 1000 --cycle same --deadline-us 300 --spin-us 0`

use std::io;
use std::process::ExitCode;

use keel_control::arg;
use keel_control::joints::{self, Settings};
use keel_control::plant::Pendulums;

fn main() -> ExitCode {
    joints::exit_on(main_())
}

fn main_() -> io::Result<()> {
    let settings = Settings::from_args()?;
    joints::run(Pendulums::new(arg("joints", 1)?, settings.hz), settings)
}
