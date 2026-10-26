# 0034: Position servos on a serial bus: keel-dynamixel

**Status:** Accepted, 2026-10-26

## Context
The second milestone is Pollen's Microduck walking: a 25 cm biped with
fifteen Dynamixel XL330 servos on one half-duplex serial bus at 1 Mbit/s
(Protocol 2.0), and a body IMU that answers on the same bus as one more
device (id 200). Pollen's own runtime drives it at 50 Hz: each tick, one
Fast Sync Read of twelve bytes at address 124 from the IMU and every servo
(PWM, current, velocity, position), then one Sync Write of goal positions.

drydock now plays the duck behind a pseudo-terminal, byte for byte, as it
plays OpenArm behind `vcan0`, and Pollen's servo library (rustypot) reads it
unchanged.

An XL330 isn't a Damiao motor. It takes a goal position, and closes its own
loop with its own P gain, a register; it doesn't take a velocity, gains or
a torque per command.

## Decision
- **A node, `keel-dynamixel`**, for a serial bus of Dynamixel servos:
  `--port` (the robot's `/dev/ttyS2`, or drydock's link), `--baud`, and
  `--motors`, the servos' ids in joint order. It's a driver behind
  `JointLoop` ([0032](0032-a-loop-for-any-joints.md)), as `DamiaoJoints`
  is, so health checks and the agent's tools work as they do on OpenArm.
- **A command's position is the servo's goal.** Its velocity, gains and
  effort are not used: the servo's P gain is a register, set once at start
  (`--p-gain`, 200 on the Microduck, as Pollen sets it), and writing gains
  each tick would cost bus time the duck doesn't have. The node says so
  once if a controller sends any.
- **One read and one write per cycle**, as Pollen's runtime: a Fast Sync
  Read of PWM, current, velocity and position for every servo (and the IMU's
  block, [0035](0035-imu-messages.md)), then a Sync Write of the goals.
- **A state's effort** is the servo's current times its torque constant
  (BAM's measured 0.366 N m/A); its status says torque off as disabled, and
  a hardware error as the drive's error, its code the Hardware Error Status.
- **keel touches the servos' RAM only**: torque, the P gain, goals. How a
  robot's servos are set up (ids, baud rate, return delay, Shutdown, homing
  offsets) is the robot's EEPROM, as a robot's CAN ids are; the node checks
  what it needs and says what's wrong, with the value to write.
- **keel has its own copy of Protocol 2.0** (packets, stuffing, CRC), and
  opens the port itself (termios, through `libc`), as it does SocketCAN: a
  robot's stack shouldn't depend on its simulator, nor on a serial crate for
  what is a few system calls.

## Consequences
- The Microduck's joints in keel, against drydock (`--lockstep 50`), then
  the duck itself.
- Controllers written for MIT commands don't drive a servo the same way: a
  torque (`keel-pid`'s `effort`) does nothing on it. A controller for servos
  sends positions.
- The XL330's other modes (velocity, current, PWM) aren't used.
