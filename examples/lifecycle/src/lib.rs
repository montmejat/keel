//! Nodes that misbehave on purpose, to show restarts and the watchdog
//! (`examples/lifecycle.yml`). Messages are a `u64` count, little-endian.

pub fn count(data: &[u8]) -> u64 {
    u64::from_le_bytes(data[..8].try_into().unwrap())
}
