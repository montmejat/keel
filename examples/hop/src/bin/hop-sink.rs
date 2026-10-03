//! Measures each message from `hop-source`: its publish time, from its trace
//! context, to the moment `next_event` hands it over. Prints percentiles
//! once the source is done.
//!
//! `--spin off|forever|<µs>`: how long to watch for a message before
//! sleeping. `--touch`: also read the whole payload, one byte per cache line.

use hop::{arg, report, spin_arg};
use keel::trace::now_ns;
use keel::{Event, Node};

fn main() -> std::io::Result<()> {
    let spin = spin_arg();
    let touch = std::env::args().any(|a| a == "--touch");
    let label: String = arg("label", format!("keel, spin {}", arg("spin", String::from("off"))));
    let mut node = Node::from_env()?;
    node.set_spin(spin);
    let mut hops = Vec::with_capacity(100_000);
    let mut reads = Vec::with_capacity(100_000);
    let mut sum = 0u64;
    while let Event::Input { data, .. } = node.next_event()? {
        let now = now_ns();
        hops.push(now.saturating_sub(data.context().published_ns));
        if touch {
            sum += data.iter().step_by(64).map(|&b| b as u64).sum::<u64>();
            reads.push(now_ns() - now);
        }
    }
    report(&label, &mut hops);
    if touch {
        report(&format!("{label}, read payload"), &mut reads);
        std::hint::black_box(sum);
    }
    Ok(())
}
