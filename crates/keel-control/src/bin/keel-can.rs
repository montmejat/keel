//! Damiao's motors on a CAN bus, as joints: the joints' loop
//! ([`keel_control::joints::JointLoop`]) over
//! [`keel_control::damiao::DamiaoJoints`]. Once per period it publishes what
//! the motors reported, waits up to `--deadline-us` for the command that
//! answers it, and sends it straight away (see [`keel_control::cycle`]). A
//! command that misses the deadline goes out at the next tick; meanwhile the
//! motors get the previous one again. `--cycle next` sends every command at
//! the next tick: up to a period of delay. Either way, the motors get one
//! round of frames per cycle, and the loop closes on them (MIT mode).
//!
//! `--motors` lists each motor's CAN id and the id it answers on, in joint
//! order; `--fd true` for motors on CAN FD. Motors that don't answer yet are
//! waited for. `keel doctor` checks that the interface is there (and carries
//! CAN FD if asked), and whether the motors answer.
//!
//! `--interface can0 --motors 0x01:0x11,0x02:0x12 --fd false --hz 1000 --cycle same --deadline-us 300 --spin-us 0 --names a,b`

use std::io;
use std::process::ExitCode;

use keel_control::arg;
use keel_control::cycle::Cycle;
use keel_control::damiao::DamiaoJoints;
use keel_control::joints::{self, Settings};

fn main() -> ExitCode {
    joints::exit_on(main_())
}

fn main_() -> io::Result<()> {
    let interface: String = arg("interface", "can0".into())?;
    let settings = Settings::from_args()?;
    if settings.cycle == Cycle::Lockstep {
        return Err(io::Error::other("--cycle lockstep is for simulations: a bus keeps time"));
    }
    let motors = DamiaoJoints::parse_motors(&arg("motors", String::new())?)?;
    joints::run(DamiaoJoints::open(&interface, &motors, arg("fd", false)?)?, settings)
}
