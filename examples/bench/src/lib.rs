//! Shared by `bench-source` and `bench-sink`. The first payload byte says
//! what the sink should do with a message.

/// Echo the message back right away (latency).
pub const PING: u8 = 0;
/// Swallow the message (throughput).
pub const BULK: u8 = 1;
/// Last bulk message: reply once, so the source knows all of them arrived.
pub const BULK_LAST: u8 = 2;
