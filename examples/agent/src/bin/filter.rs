//! Passes readings on, and goes wrong on purpose after `--after` of them, in
//! the way `--fault` says:
//!
//! - `slow`: takes 15 ms to process each one, more than the 10 ms between them
//! - `stall`: stops taking readings but stays alive: no crash, no log line
//! - `crash`: exits with an error, so that its restart policy brings it back
//!   to do the same again
//!
//! `--fault none --after 300`

use std::time::Duration;

use keel::{Event, Node};
use keel_control::arg;

fn main() -> std::io::Result<()> {
    let fault: String = arg("fault", "none".into())?;
    let after: u64 = arg("after", 300)?;
    let mut node = Node::from_env()?;
    let mut seen = 0u64;
    while let Event::Input { data, .. } = node.next_event()? {
        seen += 1;
        if seen > after {
            match fault.as_str() {
                "slow" => std::thread::sleep(Duration::from_millis(15)),
                "stall" => loop {
                    std::thread::sleep(Duration::from_secs(3600));
                },
                "crash" => {
                    eprintln!("filter: unexpected reading at {seen}, giving up");
                    std::process::exit(1);
                }
                _ => {}
            }
        }
        node.send_output("filtered", &data)?;
    }
    Ok(())
}
