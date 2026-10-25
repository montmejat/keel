//! Joints on a CAN bus: the joints' loop ([`keel_control::joints::JointLoop`])
//! over the drives. Once per period it publishes what the drives reported,
//! waits up to `--deadline-us` for the command that answers it, and sends it
//! straight away (see [`keel_control::cycle`]). A command that misses the
//! deadline goes out at the next tick; meanwhile the drives get the previous
//! one again. `--cycle next` sends every command at the next tick: up to a
//! period of delay. Either way, the drives get one round of frames per cycle.
//!
//! Two kinds of drives (`--protocol`):
//!
//! - `made-up` (default): `keel-can-motor`'s frames, a torque per joint
//!   ([`keel_control::can::CanJoints`]): the command's law is worked out
//!   here, once a cycle.
//! - `damiao`: Damiao's motors in MIT mode
//!   ([`keel_control::damiao::DamiaoJoints`]): the loop closes on the
//!   motors. `--motors` lists each motor's CAN id and the id it answers on,
//!   in joint order; `--fd true` for motors on CAN FD. Motors that don't
//!   answer yet are waited for.
//!
//! `keel doctor` checks that the interface is there (and carries CAN FD if
//! asked), and whether the motors answer.
//!
//! `--interface can0 --joints 1 --hz 1000 --cycle same --deadline-us 300 --spin-us 0`
//! `--protocol damiao --motors 0x01:0x11,0x02:0x12 --fd true`

use std::io;
use std::process::ExitCode;

use keel_control::arg;
use keel_control::can::CanJoints;
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
    match arg("protocol", String::from("made-up"))?.as_str() {
        "made-up" => joints::run(CanJoints::open(&interface, arg("joints", 1)?)?, settings),
        "damiao" => {
            let motors = DamiaoJoints::parse_motors(&arg("motors", String::new())?)?;
            joints::run(DamiaoJoints::open(&interface, &motors, arg("fd", false)?)?, settings)
        }
        other => Err(io::Error::other(format!("--protocol: `made-up` or `damiao`, not {other:?}"))),
    }
}
