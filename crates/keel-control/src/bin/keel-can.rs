//! Joints on a CAN bus: sends each `command` as frames, and publishes the
//! `state` the drives report. A cyclic bus master: once per period it
//! publishes what the drives reported, stamped with when it read them, waits
//! up to `--deadline-us` for the command that answers it, and sends that
//! straight away (see [`keel_control::cycle`]). A command that misses the
//! deadline goes out at the next tick, and the drives keep the previous one
//! meanwhile. `--cycle next` sends every command at the next tick: up to a
//! period of delay.
//!
//! `--interface can0 --joints 1 --hz 1000 --cycle same --deadline-us 300 --spin-us 0`

use std::io;
use std::time::{Duration, Instant};

use keel::trace::now_ns;
use keel::{Event, Node, Periodic};
use keel_control::can::{self, Bus};
use keel_control::cycle::{self, Answer, Cycle, Misses};
use keel_control::{arg, Command, State};

/// Drives silent for this long get a line in the log.
const SILENCE: Duration = Duration::from_secs(1);

fn main() -> io::Result<()> {
    let interface: String = arg("interface", "can0".into())?;
    let joints: usize = arg("joints", 1)?;
    if joints > can::MAX_JOINTS {
        return Err(io::Error::other(format!("a bus carries at most {} joints", can::MAX_JOINTS)));
    }
    let period = Duration::from_secs_f64(1.0 / arg("hz", 1000.0)?);
    let same_cycle = match cycle::cycle_arg()? {
        Cycle::Same => true,
        Cycle::Next => false,
        Cycle::Lockstep => return Err(io::Error::other("--cycle lockstep is for simulations: a bus keeps time")),
    };
    let deadline = Duration::from_micros(arg("deadline-us", 300)?);
    let bus = Bus::open(&interface)?;
    let mut node = Node::from_env()?;
    node.set_spin(Duration::from_micros(arg("spin-us", 0)?));
    // Nothing to wait for without a controller.
    let same_cycle = same_cycle && !node.inputs().is_empty();
    let mut misses = Misses::new(deadline);
    let mut states = vec![State::default(); joints];
    // Which joints have reported since the last `state`.
    let mut fresh = vec![false; joints];
    let (mut heard, mut complained) = (Instant::now(), false);

    let mut periodic = Periodic::new(period);
    let send = |data: &[u8]| -> io::Result<()> {
        for (joint, command) in Command::read(data).take(joints).enumerate() {
            bus.send(&can::command_frame(joint, command))?;
        }
        Ok(())
    };
    loop {
        periodic.wait();
        // Between ticks only late commands arrive in same-cycle mode, and
        // every command in next-cycle mode: either way, on to the drives.
        while let Some(event) = node.try_next_event()? {
            match event {
                Event::Input { data, .. } => send(&data)?,
                Event::Stop => return Ok(()),
            }
        }
        let read_ns = now_ns();
        while let Some(frame) = bus.try_recv()? {
            if let Some((joint, state)) = can::as_state(&frame, joints) {
                (states[joint], fresh[joint]) = (state, true);
            }
        }
        // A state is every joint's: nothing is published from half a bus.
        if fresh.iter().all(|&f| f) {
            fresh.fill(false);
            if complained {
                println!("`{interface}` is back");
            }
            (heard, complained) = (Instant::now(), false);
            node.send_stamped("state", read_ns, joints * State::LEN, |payload| State::write(&states, payload))?;
            if same_cycle {
                let asked = node.last_sent_span();
                let answer = cycle::wait_for_answer(&mut node, asked, Some(deadline), send)?;
                if answer == Answer::Stop {
                    return Ok(());
                }
                misses.count(answer);
            }
        } else if !complained && heard.elapsed() >= SILENCE {
            complained = true;
            println!("no state from every joint on `{interface}` for {SILENCE:?}");
        }
    }
}
