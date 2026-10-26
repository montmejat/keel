# 0035: IMU messages

**Status:** Accepted, 2026-10-26

## Context
keel's payloads so far are joints: `State` and `Command`. A walking robot's
policy also needs the body's motion: its angular velocity and which way is
down. On the Microduck, the IMU (an LSM6DSV16X) is a device on the servos'
bus, read in the same transaction, and its chip fuses its own orientation
(a "game rotation vector": no magnetometer, so its heading is arbitrary).
Pollen's board sends the gyro and that orientation each tick; its
accelerometer only in a longer block the runtime doesn't read each tick.

## Decision
- **An `Imu` payload**: an orientation (a unit quaternion, w x y z, the
  body's frame in a world whose z is up), an angular velocity (rad/s) and a
  linear acceleration (m/s², gravity included: +9.81 up at rest), both in
  the body's frame. Ten `f64`, 80 bytes. What a sensor doesn't measure is
  NaN: the duck's tick read has no acceleration.
- **The body's frame, not the chip's.** The node that reads the IMU turns
  its readings by how it's mounted (`--imu-mount`, a quaternion), so a
  controller sees the trunk, x forward, y left, z up, as Pollen's runtime
  does.
- **Published by the node that reads it**, on its own output, with the
  joints' stamp when it's read in the same transaction: `keel-dynamixel`
  publishes `state` and `imu`.

## Consequences
- A policy reads gravity in the body's frame from the orientation, and the
  gyro as it is.
- The layout, like `State`'s, is part of what must not break.
