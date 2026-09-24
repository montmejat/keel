//! Summarises the brightness reports every five seconds.

use keel::{Event, Node};
use pipeline::FPS;

fn main() -> std::io::Result<()> {
    let mut node = Node::from_env()?;
    let (mut count, mut sum, mut last_frame) = (0u64, 0u64, 0u64);
    while let Event::Input { data, .. } = node.next_event()? {
        last_frame = u64::from_le_bytes(data[..8].try_into().unwrap());
        sum += u64::from_le_bytes(data[8..16].try_into().unwrap());
        count += 1;
        if count % u64::from(FPS * 5) == 0 {
            println!("{count} reports, up to frame {last_frame}, mean brightness {}", sum / count);
        }
    }
    println!("recorded {count} reports, last frame {last_frame}");
    Ok(())
}
