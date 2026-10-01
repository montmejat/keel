//! Stands in for a microcontroller running `keel-micro`: what the chip sends
//! on channel `n` is published on this node's `n`th output, and what its
//! `n`th input receives is sent to the chip on channel `n`. Payloads go
//! through untouched.
//!
//! `--port /dev/ttyACM0 --baud 115200 --outputs state,temperature --inputs command --hz 1000`

use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use keel::{Event, Node, Periodic};
use keel_control::arg;
use keel_serial::Port;

/// The longest message, either way.
const MAX_FRAME: usize = 4096;
/// How long the port gets to appear: a chip takes a moment to enumerate.
const PORT_TIMEOUT: Duration = Duration::from_secs(5);

fn main() -> io::Result<()> {
    let device: PathBuf = arg("port", "/dev/ttyACM0".into())?;
    let baud: u32 = arg("baud", 115200)?;
    let period = Duration::from_secs_f64(1.0 / arg("hz", 1000.0)?);
    let names = |list: String| list.split(',').filter(|n| !n.is_empty()).map(str::to_owned).collect::<Vec<_>>();
    let (outputs, inputs) = (names(arg("outputs", String::new())?), names(arg("inputs", String::new())?));

    let deadline = Instant::now() + PORT_TIMEOUT;
    let port = loop {
        match Port::open(&device, baud) {
            Ok(port) => break port,
            Err(e) if Instant::now() > deadline => return Err(e),
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    };
    let mut chip: keel_micro::Node<Port, MAX_FRAME> = keel_micro::Node::new(port);
    let mut node = Node::from_env()?;
    println!("{}: channels out {outputs:?}, in {inputs:?}", device.display());

    let mut periodic = Periodic::new(period);
    loop {
        periodic.wait();
        while let Some(event) = node.try_next_event()? {
            match event {
                Event::Input { id, data } => {
                    if let Some(channel) = inputs.iter().position(|input| input == id) {
                        chip.send(channel as u8, &data);
                    }
                }
                Event::Stop => return Ok(()),
            }
        }
        while let Some((channel, payload)) = chip.poll() {
            if let Some(output) = outputs.get(channel as usize) {
                node.send_output(output, payload)?;
            }
        }
    }
}
