//! Estimates each frame's brightness from a sample of its pixels, in place,
//! and reports when the scene turns bright or dark.

use keel::{Event, Node};
use pipeline::HEADER_LEN;

const BRIGHT: u64 = 200;
const DARK: u64 = 60;

fn main() -> std::io::Result<()> {
    let mut node = Node::from_env()?;
    let mut bright = None;
    while let Event::Input { data, .. } = node.next_event()? {
        let frame = u64::from_le_bytes(data[..HEADER_LEN].try_into().unwrap());
        let pixels = &data[HEADER_LEN..];
        let samples = pixels.iter().step_by(4099);
        let brightness = samples.clone().map(|&p| u64::from(p)).sum::<u64>() / samples.count() as u64;

        let now_bright = match brightness {
            b if b >= BRIGHT => Some(true),
            b if b <= DARK => Some(false),
            _ => bright,
        };
        if now_bright != bright && now_bright.is_some() {
            let scene = if now_bright == Some(true) { "bright" } else { "dark" };
            println!("frame {frame}: scene turned {scene} ({brightness})");
            bright = now_bright;
        }
        let report = [frame.to_le_bytes(), brightness.to_le_bytes()].concat();
        node.send_output("brightness", &report)?;
    }
    println!("no more frames, stopping");
    Ok(())
}
