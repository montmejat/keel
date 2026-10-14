//! Simulated devices: the same kinds as real ones ([`crate::device`]),
//! over a shared [`World`]. The world is a physics engine ([`Physics`]: true
//! values, nothing else) and the joints' drives ([`crate::drive`]); each
//! device reads it through its own hardware model ([`crate::model`]), so it
//! reads, lags and breaks like the real thing.
//!
//! The devices share the world, so they run in one node, on the world's
//! clock ([`SimClock`]): it steps the physics up to each tick, and in real
//! time waits for the wall clock to catch up; in lockstep it doesn't wait.

use std::cell::RefCell;
use std::hash::{Hash, Hasher};
use std::io;
use std::rc::Rc;

use keel::trace::now_ns;

use crate::device::{Camera, Joints, VectorSensor};
use crate::drive::{Drive, DriveSettings};
use crate::model::{ImageModel, ImageSettings, VectorModel, VectorSettings};
use crate::plant::Pendulum;
use crate::schedule::{sleep_until, Clock};
use crate::{Command, Format, Image, Mode, State};

/// A physics engine: the truth, with no idea that sensors exist.
pub trait Physics {
    /// Simulated time since the start.
    fn time_ns(&self) -> u64;
    fn timestep_ns(&self) -> u64;

    /// The joints it drives, one per actuator.
    fn joints(&self) -> usize;
    fn joint(&self, i: usize) -> State;
    fn set_torque(&mut self, i: usize, torque: f64);

    /// One timestep, under the torques set.
    fn step(&mut self);

    /// A sensor's true values, appended to `out`.
    fn sensor(&self, name: &str, _out: &mut Vec<f64>) -> io::Result<()> {
        Err(io::Error::other(format!("no sensor {name:?}: this world has none")))
    }

    /// What a camera sees, into `image` (whose size and format say what's
    /// wanted).
    fn render(&mut self, camera: &str, _image: &mut Image) -> io::Result<()> {
        Err(io::Error::other(format!("no camera {camera:?}: this world has none")))
    }
}

/// The physics, and the drives between commands and its joints.
pub struct World {
    physics: Box<dyn Physics>,
    drives: Vec<Drive>,
    /// keel's time at the start: a simulation's stamps begin at the clock's,
    /// like a robot's.
    origin: u64,
}

pub type Shared = Rc<RefCell<World>>;

impl World {
    /// A world with a drive per joint, from `drive(joint)`.
    pub fn new(physics: Box<dyn Physics>, drive: impl Fn(usize) -> DriveSettings) -> io::Result<Shared> {
        let drives = (0..physics.joints()).map(|j| Drive::new(drive(j))).collect::<io::Result<_>>()?;
        Ok(Rc::new(RefCell::new(World { physics, drives, origin: now_ns() })))
    }

    /// The world's time, on keel's clock.
    pub fn now(&self) -> u64 {
        self.origin + self.physics.time_ns()
    }

    pub fn physics(&mut self) -> &mut dyn Physics {
        &mut *self.physics
    }

    /// One timestep: each drive's torque, then the physics.
    pub fn step(&mut self) {
        let (now, dt) = (self.now(), self.physics.timestep_ns() as f64 / 1e9);
        for (j, drive) in self.drives.iter_mut().enumerate() {
            let torque = drive.torque(now, self.physics.joint(j), dt);
            self.physics.set_torque(j, torque);
        }
        self.physics.step();
    }
}

/// The world's clock: steps the world up to each tick. In real time, it
/// also waits for keel's clock to get there; in lockstep, as fast as the
/// machine and the controller allow.
pub struct SimClock {
    world: Shared,
    realtime: bool,
}

impl SimClock {
    pub fn new(world: &Shared, realtime: bool) -> SimClock {
        SimClock { world: world.clone(), realtime }
    }
}

impl Clock for SimClock {
    fn now(&self) -> u64 {
        self.world.borrow().now()
    }

    fn advance_to(&mut self, t: u64) -> io::Result<()> {
        let mut world = self.world.borrow_mut();
        while world.now() < t {
            world.step();
        }
        drop(world);
        if self.realtime {
            sleep_until(t);
        }
        Ok(())
    }
}

/// A seed for a device's noise, from its name: every device its own, the
/// same every run.
pub fn seed(name: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    name.hash(&mut hasher);
    hasher.finish()
}

/// The world's joints, through their drives and encoders.
pub struct SimJoints {
    world: Shared,
    encoders: Vec<VectorModel>,
}

impl SimJoints {
    /// With an encoder model per joint, from `encoder(joint)`.
    pub fn new(world: &Shared, encoder: impl Fn(usize) -> VectorSettings) -> io::Result<SimJoints> {
        let n = world.borrow().physics.joints();
        let encoders = (0..n)
            .map(|j| VectorModel::new(encoder(j), seed(&format!("joint{}", j + 1))))
            .collect::<io::Result<_>>()?;
        Ok(SimJoints { world: world.clone(), encoders })
    }
}

impl Joints for SimJoints {
    fn count(&self) -> usize {
        self.encoders.len()
    }

    fn read(&mut self, states: &mut [State]) -> io::Result<Option<u64>> {
        let world = self.world.borrow();
        let now = world.now();
        let mut read = [0.0; 2];
        for (j, (state, encoder)) in states.iter_mut().zip(&mut self.encoders).enumerate() {
            let truth = world.physics.joint(j);
            encoder.apply(now, &[truth.position, truth.velocity], &mut read);
            *state = State { position: read[0], velocity: read[1] };
        }
        Ok(Some(now))
    }

    fn write(&mut self, commands: &[Command]) -> io::Result<()> {
        let mut world = self.world.borrow_mut();
        let now = world.now();
        for (drive, command) in world.drives.iter_mut().zip(commands) {
            drive.command(now, *command);
        }
        Ok(())
    }

    /// The drive model closes every loop.
    fn closes(&self, _mode: Mode) -> bool {
        true
    }
}

/// A numeric sensor: the world's true values for one or more of its sensors
/// (an accelerometer and a gyro make an IMU), through a sensor model.
pub struct SimSensor {
    world: Shared,
    names: Vec<String>,
    model: VectorModel,
    truth: Vec<f64>,
}

impl SimSensor {
    pub fn new(world: &Shared, name: &str, sensors: &[String], settings: VectorSettings) -> io::Result<SimSensor> {
        let mut sensor = SimSensor {
            world: world.clone(),
            names: sensors.to_vec(),
            model: VectorModel::new(settings, seed(name))?,
            truth: Vec::new(),
        };
        sensor.fetch()?;
        Ok(sensor)
    }

    fn fetch(&mut self) -> io::Result<()> {
        self.truth.clear();
        let world = self.world.borrow();
        for name in &self.names {
            world.physics.sensor(name, &mut self.truth)?;
        }
        Ok(())
    }
}

impl VectorSensor for SimSensor {
    fn count(&self) -> usize {
        self.truth.len()
    }

    fn read(&mut self, values: &mut [f64]) -> io::Result<Option<u64>> {
        self.fetch()?;
        let now = self.world.borrow().now();
        self.model.apply(now, &self.truth, values);
        Ok(Some(now))
    }
}

/// A camera: what the world renders, through a camera model.
pub struct SimCamera {
    world: Shared,
    camera: String,
    width: u32,
    height: u32,
    format: Format,
    model: ImageModel,
}

impl SimCamera {
    pub fn new(
        world: &Shared,
        name: &str,
        camera: &str,
        (width, height): (u32, u32),
        format: Format,
        settings: ImageSettings,
    ) -> io::Result<SimCamera> {
        let model = ImageModel::new(settings, seed(name))?;
        Ok(SimCamera { world: world.clone(), camera: camera.into(), width, height, format, model })
    }
}

impl Camera for SimCamera {
    fn capture(&mut self, image: &mut Image) -> io::Result<Option<u64>> {
        if (image.width, image.height, image.format) != (self.width, self.height, self.format) {
            *image = Image::new(self.width, self.height, self.format);
        }
        let mut world = self.world.borrow_mut();
        let now = world.now();
        world.physics.render(&self.camera, image)?;
        drop(world);
        self.model.apply(now, image);
        Ok(Some(now))
    }
}

/// The simplest physics there is: a pendulum per joint (see
/// [`crate::plant`]). No sensors, no cameras.
pub struct Pendulums {
    plants: Vec<Pendulum>,
    torques: Vec<f64>,
    time: u64,
    timestep: u64,
}

impl Pendulums {
    pub fn new(joints: usize, timestep_ns: u64) -> Pendulums {
        Pendulums {
            plants: vec![Pendulum::default(); joints],
            torques: vec![0.0; joints],
            time: 0,
            timestep: timestep_ns,
        }
    }
}

impl Physics for Pendulums {
    fn time_ns(&self) -> u64 {
        self.time
    }

    fn timestep_ns(&self) -> u64 {
        self.timestep
    }

    fn joints(&self) -> usize {
        self.plants.len()
    }

    fn joint(&self, i: usize) -> State {
        self.plants[i].state()
    }

    fn set_torque(&mut self, i: usize, torque: f64) {
        self.torques[i] = torque;
    }

    fn step(&mut self) {
        let dt = self.timestep as f64 / 1e9;
        self.plants.iter_mut().zip(&self.torques).for_each(|(p, &t)| p.step(t, dt));
        self.time += self.timestep;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A position command through the simulated joints: the drive model
    /// holds the pendulum there, and the encoder reads it.
    #[test]
    fn simulated_joints_hold_a_position() {
        let world = World::new(Box::new(Pendulums::new(2, 1_000_000)), |_| DriveSettings::default()).unwrap();
        let mut joints = SimJoints::new(&world, |_| VectorSettings { quantum: 0.001, ..Default::default() }).unwrap();
        let mut clock = SimClock::new(&world, false);
        let mut states = [State::default(); 2];
        let start = clock.now();
        for tick in 1..=3000u64 {
            joints.write(&[Command::position(0.5), Command::effort(0.0)]).unwrap();
            clock.advance_to(start + tick * 1_000_000).unwrap();
        }
        joints.read(&mut states).unwrap();
        assert!((states[0].position - 0.5).abs() < 0.005, "{states:?}");
        assert_eq!(states[1].position, 0.0, "the other hangs");
    }
}
