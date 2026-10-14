//! A trait per kind of device. A real driver implements one ([`crate::can`]'s
//! joints on a bus, an IMU's driver, a camera's); so does a simulated device
//! ([`crate::sim`]). The loop for each kind ([`crate::schedule`]) runs any of
//! them, so a driver is only what its device needs: open, read, write.
//!
//! Every reading comes with its stamp: when it was taken, on keel's clock
//! ([`keel::trace::now_ns`]) or on a simulation's.

use std::io;

use crate::{Command, Image, Mode, State};

/// Joints: read every joint's state, write every joint's command.
pub trait Joints {
    fn count(&self) -> usize;

    /// Every joint's state, and when it was read; `None` until there's a
    /// complete set (on a bus, until every drive has reported).
    fn read(&mut self, states: &mut [State]) -> io::Result<Option<u64>>;

    fn write(&mut self, commands: &[Command]) -> io::Result<()>;

    /// Whether the device closes the loop for `mode` itself: if not, the
    /// joints' loop must (see [`crate::schedule::LoopAt`]). Efforts it takes
    /// as they are.
    fn closes(&self, mode: Mode) -> bool;
}

/// A sensor whose reading is a few numbers: an IMU, a force/torque sensor, a
/// rangefinder.
pub trait VectorSensor {
    /// How many numbers a reading has.
    fn count(&self) -> usize;

    /// The latest reading, and when it was taken; `None` if there's no new
    /// one.
    fn read(&mut self, values: &mut [f64]) -> io::Result<Option<u64>>;
}

/// A camera: colour, or depth.
pub trait Camera {
    /// The latest picture, into `image`, and when it was taken; `None` if
    /// there's no new one.
    fn capture(&mut self, image: &mut Image) -> io::Result<Option<u64>>;
}
