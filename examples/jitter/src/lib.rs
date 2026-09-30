//! Shared by `jitter-ticker` and `jitter-echo`.

/// Size of each message, small enough that only the middleware is measured.
pub const MESSAGE_LEN: usize = 64;
