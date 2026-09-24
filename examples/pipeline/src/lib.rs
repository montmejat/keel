//! A camera → detector → recorder pipeline that runs until stopped.
//!
//! Frames are `[frame number: u64 LE][pixels: WIDTH × HEIGHT × 3]`.

pub const WIDTH: usize = 1920;
pub const HEIGHT: usize = 1080;
pub const HEADER_LEN: usize = 8;
pub const FRAME_LEN: usize = HEADER_LEN + WIDTH * HEIGHT * 3;
pub const FPS: u32 = 30;
