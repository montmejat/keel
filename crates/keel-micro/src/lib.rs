//! A keel node on a microcontroller.
//!
//! No operating system, no allocation, no dependencies: a chip joins a
//! dataflow through a byte link (a UART, USB serial) to a machine where
//! `keel-serial` stands in for it as an ordinary node. What the chip sends
//! on channel `n` comes out of that node's `n`th output; what the node's
//! `n`th input receives reaches the chip on channel `n`.
//!
//! ```ignore
//! let mut node: Node<Uart, 64> = Node::new(uart);
//! loop {
//!     while let Some((COMMAND, payload)) = node.poll() { /* drive the motor */ }
//!     node.send(STATE, &encoder.to_le_bytes());
//! }
//! ```
//!
//! On the wire a message is a frame: `[channel][payload][crc16]`, COBS
//! encoded so that it holds no zero byte, then a zero. A receiver that joins
//! mid-stream, or after noise, is back in step at the next zero, and the
//! CRC drops what the noise damaged.

#![no_std]

/// The bytes between the chip and the machine: implement it over a UART.
pub trait Link {
    /// The next received byte, if one is waiting. Must not block.
    fn read(&mut self) -> Option<u8>;
    /// Sends all of `bytes`.
    fn write(&mut self, bytes: &[u8]);
}

/// Bytes a frame adds to its payload, before encoding: channel and CRC.
pub const OVERHEAD: usize = 3;

/// A node whose frames, before encoding, fit in `N` bytes: payloads of up to
/// `N - OVERHEAD`.
pub struct Node<L: Link, const N: usize> {
    link: L,
    /// The frame being received, decoded as it comes.
    rx: [u8; N],
    /// Its length so far; past `N`, it's too long and will be dropped.
    rx_len: usize,
    /// Bytes left in the run being received, and whether a zero follows it.
    run: (u8, bool),
    tx: [u8; N],
}

impl<L: Link, const N: usize> Node<L, N> {
    pub fn new(link: L) -> Self {
        Self { link, rx: [0; N], rx_len: 0, run: (0, false), tx: [0; N] }
    }

    /// Sends `payload` on `channel`. `false` if it doesn't fit in a frame.
    pub fn send(&mut self, channel: u8, payload: &[u8]) -> bool {
        let len = payload.len() + OVERHEAD;
        if len > N {
            return false;
        }
        self.tx[0] = channel;
        self.tx[1..len - 2].copy_from_slice(payload);
        let crc = crc16(&self.tx[..len - 2]);
        self.tx[len - 2..len].copy_from_slice(&crc.to_le_bytes());
        // COBS: each run of non-zero bytes goes out behind its length plus
        // one; a run of 254 is followed by another instead of a zero.
        let frame = &self.tx[..len];
        let mut at = 0;
        loop {
            let run = frame[at..].iter().take(254).take_while(|&&b| b != 0).count();
            self.link.write(&[run as u8 + 1]);
            self.link.write(&frame[at..at + run]);
            at += run;
            if run < 254 {
                if at == len {
                    break;
                }
                at += 1; // the zero this run ended on
            } else if at == len {
                break;
            }
        }
        self.link.write(&[0]);
        true
    }

    /// Reads what the link has, up to the end of a frame: the channel and
    /// payload of the next message that arrived whole, or `None` once the
    /// link is empty. Call it until it returns `None`.
    pub fn poll(&mut self) -> Option<(u8, &[u8])> {
        while let Some(byte) = self.link.read() {
            let (left, zero_after) = self.run;
            if byte == 0 {
                // The end of a frame: whole if its last run was, and its CRC
                // holds.
                let len = core::mem::take(&mut self.rx_len);
                self.run = (0, false);
                if left != 0 || len < OVERHEAD || len > N {
                    continue;
                }
                let (body, crc) = self.rx[..len].split_at(len - 2);
                if crc16(body).to_le_bytes() != crc {
                    continue;
                }
                return Some((self.rx[0], &self.rx[1..len - 2]));
            }
            if left > 0 {
                self.push(byte);
                self.run.0 = left - 1;
            } else {
                // A run's length: the previous run's zero comes first.
                if zero_after {
                    self.push(0);
                }
                self.run = (byte - 1, byte != 255);
            }
        }
        None
    }

    fn push(&mut self, byte: u8) {
        if self.rx_len < N {
            self.rx[self.rx_len] = byte;
        }
        self.rx_len = self.rx_len.saturating_add(1);
    }

    /// The link, e.g. to flush it.
    pub fn link(&mut self) -> &mut L {
        &mut self.link
    }
}

/// CRC-16/CCITT-FALSE.
fn crc16(bytes: &[u8]) -> u16 {
    let mut crc = 0xffffu16;
    for &byte in bytes {
        crc ^= u16::from(byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x1021 } else { crc << 1 };
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::collections::VecDeque;
    use std::vec::Vec;

    use super::*;

    /// A wire: what's written can be read back.
    #[derive(Default)]
    struct Loop(VecDeque<u8>);

    impl Link for Loop {
        fn read(&mut self) -> Option<u8> {
            self.0.pop_front()
        }
        fn write(&mut self, bytes: &[u8]) {
            self.0.extend(bytes);
        }
    }

    #[test]
    fn crc_check_value() {
        assert_eq!(crc16(b"123456789"), 0x29b1);
    }

    #[test]
    fn frames_round_trip() {
        let mut node: Node<Loop, 600> = Node::new(Loop::default());
        let long: Vec<u8> = (0..597u32).map(|n| (n % 255) as u8 + 1).collect();
        let payloads: [&[u8]; 6] = [b"", b"\0", b"hello", &[0, 0, 7, 0], &[1; 254], &long];
        for (channel, payload) in payloads.iter().enumerate() {
            assert!(node.send(channel as u8, payload));
        }
        assert!(!node.send(0, &[0; 598]), "too long for the buffer");
        assert!(node.link().0.iter().filter(|&&b| b == 0).count() == payloads.len(), "zeros only end frames");
        for (channel, payload) in payloads.iter().enumerate() {
            assert_eq!(node.poll(), Some((channel as u8, *payload)));
        }
        assert_eq!(node.poll(), None);
    }

    #[test]
    fn noise_costs_one_frame() {
        let mut node: Node<Loop, 32> = Node::new(Loop::default());
        // Joined mid-frame, then a flipped bit, then a frame too long.
        node.link().write(&[9, 9, 9, 0]);
        node.send(1, b"first");
        node.send(2, b"damaged");
        let at = node.link().0.len() - 4;
        node.link().0[at] ^= 0x10;
        node.link().write(&[7; 100]);
        node.link().write(&[0]);
        node.send(3, b"last");
        assert_eq!(node.poll(), Some((1, &b"first"[..])));
        assert_eq!(node.poll(), Some((3, &b"last"[..])));
        assert_eq!(node.poll(), None);
    }
}
