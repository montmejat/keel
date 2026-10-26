//! Dynamixel servos on a serial bus, as joints: the joints' loop
//! ([`keel_control::joints::JointLoop`]) over
//! [`keel_control::dynamixel::DynamixelJoints`]. Each cycle, one Fast Sync
//! Read of every servo (and the IMU board, if there's one), published as
//! `state` (and `imu`), then one Sync Write of the goals: a command's
//! position is its servo's goal (decision 0034).
//!
//! `--motors` lists the servos' ids, in joint order. `--imu 200` reads the
//! Microduck's IMU board too, its readings turned into the body's frame by
//! `--imu-mount w,x,y,z`. At start, each servo holds where it is, with
//! `--p-gain` as its P gain. Servos that don't answer yet are waited for.
//!
//! `--port /dev/ttyS2 --baud 1000000 --motors 20,21,22 --p-gain 200 --imu 200 --imu-mount 0.7071,0,0.7071,0 --hz 50`

use std::io;
use std::process::ExitCode;

use keel_control::arg;
use keel_control::dynamixel::DynamixelJoints;
use keel_control::joints::{self, Settings};

fn main() -> ExitCode {
    joints::exit_on(main_())
}

fn main_() -> io::Result<()> {
    let port: String = arg("port", "/dev/ttyS2".into())?;
    let settings = Settings::from_args()?;
    let ids = DynamixelJoints::parse_ids(&arg("motors", String::new())?)?;
    let imu: u32 = arg("imu", 0)?;
    let imu = match imu {
        0 => None,
        id => {
            let id = u8::try_from(id).map_err(|_| io::Error::other("--imu: a device id, 1 to 252"))?;
            Some((id, DynamixelJoints::parse_mount(&arg("imu-mount", "1,0,0,0".to_string())?)?))
        }
    };
    let joints = DynamixelJoints::open(&port, arg("baud", 1_000_000)?, ids, imu, arg("p-gain", 400)?)?;
    joints::run(joints, settings)
}
