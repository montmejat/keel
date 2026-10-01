# 0025: A microcontroller is a node, behind a bridge on a serial link

**Status:** Accepted, 2026-10-01 (proposed 2026-10-01)

## Context
The last hop to hardware is usually a microcontroller: it reads the
encoder and drives the motor, and has no operating system, so none of
keel's shared memory, sockets or processes. It should still be a node of
the dataflow like any other: wired in the same file, visible in the same
tools.

## Decision
- **`keel-micro`**, a `no_std` crate with no allocation and no dependency:
  `Node::send(channel, payload)` and `Node::poll()`, over a `Link` trait of
  two functions (read a byte if there is one, write bytes) that a firmware
  implements on its UART. Buffers are sized by a const parameter.
- **`keel-serial`** is the chip's stand-in on the machine it's plugged into:
  an ordinary node that passes payloads through, untouched. The chip's
  channel `n` is the node's `n`th output (`--outputs`), and the node's
  `n`th input (`--inputs`) reaches the chip on channel `n`. Names live in
  the dataflow; the chip only knows numbers.
- **Frames:** `[channel][payload][CRC-16]`, COBS-encoded and ended by a
  zero byte. No zero inside a frame means a receiver that joins mid-stream,
  or after noise, is back in step at the next zero; the CRC drops what was
  damaged. Decoded as it arrives, so the buffer is the size of the message,
  not of its encoding.
- **Nothing is acknowledged or sent again**: a control loop wants the next
  state, not the one that was lost.
- **To try it without hardware**, `keel-micro-joint` runs the firmware loop
  over a pseudo-terminal, with the simulated pendulum
  ([0021](0021-control-layer.md)) for a motor.

## Consequences
- Tried: the PID holds the pendulum through the pretend chip as through
  the simulation, at 1 kHz, with about one period from state to command.
  `keel-micro` builds for `thumbv7em-none-eabihf` (a Cortex-M4F).
- Not tried: a real chip. That needs a board's HAL for the `Link`, and a
  pseudo-terminal hides what a UART does: a 16-byte state at 1 kHz is 20
  bytes a frame, 200 kbit/s, more than 115200 baud carries.
- Messages on the chip are raw bytes, as everywhere
  ([0003](0003-raw-byte-payloads.md)); the example's use `f64`, which a
  chip without a double-precision FPU computes in software.
- Tracing stops at the bridge: a message's time on the wire and on the
  chip isn't measured, and the chip's clock isn't known.
- The firmware isn't deployed by keel: `build:` for the chip's target and
  flashing by hash (so that `keel rollback` reflashes) is the natural next
  step, with a board to flash.
