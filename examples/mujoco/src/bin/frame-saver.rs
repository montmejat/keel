//! Keeps each camera's latest picture on disk, once every `--every-s`
//! seconds: `<dir>/<input>.ppm` for colour, `<dir>/<input>.pgm` for depth
//! (white is near, black is `--far` metres or nothing). For looking at what
//! the robot sees, without a viewer. Every input is a camera.
//!
//! `--dir /tmp/keel-frames --every-s 1 --far 1.5`

use std::collections::HashMap;
use std::io::Write;
use std::time::{Duration, Instant};

use keel::{Event, Node};
use keel_control::{arg, Format, Image};

fn main() -> std::io::Result<()> {
    let dir: String = arg("dir", "/tmp/keel-frames".into())?;
    let every = Duration::from_secs_f64(arg("every-s", 1.0)?);
    let far: f32 = arg("far", 1.5)?;
    let mut node = Node::from_env()?;
    std::fs::create_dir_all(&dir)?;
    let mut saved: HashMap<&str, Instant> = HashMap::new();
    while let Event::Input { id, data } = node.next_event()? {
        let Some((width, height, format, pixels)) = Image::read(&data) else { continue };
        if saved.get(id).is_some_and(|t| t.elapsed() < every) {
            continue;
        }
        saved.insert(id, Instant::now());
        let (path, header, bytes): (String, String, Vec<u8>) = match format {
            Format::Rgb8 => (format!("{dir}/{id}.ppm"), format!("P6\n{width} {height}\n255\n"), pixels.to_vec()),
            Format::DepthF32 => {
                let grey = pixels.as_chunks::<4>().0.iter().map(|b| {
                    let d = f32::from_le_bytes(*b);
                    if d <= 0.0 {
                        0
                    } else {
                        (255.0 * (1.0 - (d / far).min(1.0))) as u8
                    }
                });
                (format!("{dir}/{id}.pgm"), format!("P5\n{width} {height}\n255\n"), grey.collect())
            }
        };
        // Whole pictures only, for whoever is reading the file.
        let new = format!("{path}.{}.new", std::process::id());
        let mut out = std::fs::File::create(&new)?;
        out.write_all(header.as_bytes())?;
        out.write_all(&bytes)?;
        std::fs::rename(&new, &path)?;
    }
    Ok(())
}
