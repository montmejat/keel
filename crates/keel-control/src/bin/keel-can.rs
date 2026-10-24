//! Joints on a CAN bus: sends each `command` to the drives, and publishes
//! the `state` they report. A cyclic bus master: once per period it
//! publishes what the drives reported, waits up to `--deadline-us` for the
//! command that answers it, and sends it to the drives straight away (see
//! [`keel_control::cycle`]). A command that misses the deadline goes out at
//! the next tick; meanwhile the drives get the previous one again.
//! `--cycle next` sends every command at the next tick: up to a period of
//! delay. Either way, the drives get one round of frames per cycle.
//!
//! Two kinds of drives (`--protocol`):
//!
//! - `made-up` (default): `keel-can-motor`'s frames, a torque per joint.
//!   Those drives take only a torque, so the command's law
//!   (`Command::torque`) is worked out here, on the latest state: the loop
//!   closes once a cycle.
//! - `damiao`: Damiao's motors in MIT mode, the command as it is: the loop
//!   closes on the drives. `--motors` lists each motor's CAN id and the id
//!   it answers on, in joint order. At start, each is put in MIT mode, its
//!   ranges are read from its registers, and it's enabled; at stop, it's
//!   disabled. The motors speak only when spoken to: each round's answers
//!   are the next state, stamped with when the round was sent. `--fd true`
//!   for motors on CAN FD.
//!
//! `--interface can0 --joints 1 --hz 1000 --cycle same --deadline-us 300 --spin-us 0`
//! `--protocol damiao --motors 0x01:0x11,0x02:0x12 --fd true`

use std::io;
use std::time::{Duration, Instant};

use keel::trace::now_ns;
use keel::{Event, Node, Periodic};
use keel_control::can::{self, Bus, Frame};
use keel_control::cycle::{self, Answer, Cycle, Misses};
use keel_control::damiao::{self, reg, Limits};
use keel_control::{arg, Command, State};

/// Drives silent for this long get a line in the log.
const SILENCE: Duration = Duration::from_secs(1);
/// How long a Damiao motor has to answer a register request at start.
const REGISTER_TIMEOUT: Duration = Duration::from_millis(100);

fn main() -> io::Result<()> {
    let interface: String = arg("interface", "can0".into())?;
    let period = Duration::from_secs_f64(1.0 / arg("hz", 1000.0)?);
    let same_cycle = match cycle::cycle_arg()? {
        Cycle::Same => true,
        Cycle::Next => false,
        Cycle::Lockstep => return Err(io::Error::other("--cycle lockstep is for simulations: a bus keeps time")),
    };
    let deadline = Duration::from_micros(arg("deadline-us", 300)?);
    let mut drives = match arg("protocol", String::from("made-up"))?.as_str() {
        "made-up" => {
            let joints: usize = arg("joints", 1)?;
            if joints > can::MAX_JOINTS {
                return Err(io::Error::other(format!("a bus carries at most {} joints", can::MAX_JOINTS)));
            }
            Drives::MadeUp { joints }
        }
        "damiao" => Drives::Damiao(Damiao::from_args(&arg("motors", String::new())?, arg("fd", false)?)?),
        other => return Err(io::Error::other(format!("--protocol: `made-up` or `damiao`, not {other:?}"))),
    };
    let bus = if drives.fd() { Bus::open_fd(&interface)? } else { Bus::open(&interface)? };
    let mut node = Node::from_env()?;
    node.set_spin(Duration::from_micros(arg("spin-us", 0)?));
    drives.start(&bus, &interface)?;
    // Nothing to wait for without a controller.
    let same_cycle = same_cycle && !node.inputs().is_empty();
    let joints = drives.joints();
    let mut misses = Misses::new(deadline);
    let mut states = vec![State::default(); joints];
    // Which joints have reported since the last `state`.
    let mut fresh = vec![false; joints];
    // The newest command, and whether there's been one.
    let (mut commands, mut commanded) = (vec![Command::default(); joints], false);
    // When the last round of frames went out.
    let mut round_ns = now_ns();
    let (mut heard, mut complained) = (Instant::now(), false);

    let mut periodic = Periodic::new(period);
    loop {
        periodic.wait();
        let read_ns = now_ns();
        while let Some(frame) = bus.try_recv()? {
            if let Some(joint) = drives.absorb(&frame, &mut states) {
                fresh[joint] = true;
            }
        }
        // Commands that came between ticks: late, or every one in
        // next-cycle mode. The newest goes out with this cycle's round.
        while let Some(event) = node.try_next_event()? {
            match event {
                Event::Input { data, .. } => take(&data, &mut commands, &mut commanded),
                Event::Stop => return drives.stop(&bus),
            }
        }
        // A state is every joint's: nothing is published from half a bus.
        if fresh.iter().all(|&f| f) {
            fresh.fill(false);
            if complained {
                println!("`{interface}` is back");
            }
            (heard, complained) = (Instant::now(), false);
            let stamp = drives.stamp(read_ns, round_ns);
            node.send_stamped("state", stamp, joints * State::LEN, |payload| State::write(&states, payload))?;
            if same_cycle {
                let asked = node.last_sent_span();
                let answer = cycle::wait_for_answer(&mut node, asked, Some(deadline), |data| {
                    take(data, &mut commands, &mut commanded);
                    Ok(())
                })?;
                if answer == Answer::Stop {
                    return drives.stop(&bus);
                }
                misses.count(answer);
            }
        } else if !complained && heard.elapsed() >= SILENCE {
            complained = true;
            println!("no state from every joint on `{interface}` for {SILENCE:?}");
        }
        round_ns = now_ns();
        drives.send(&bus, &commands, commanded, &states)?;
    }
}

/// Keeps the newest command.
fn take(data: &[u8], commands: &mut [Command], commanded: &mut bool) {
    commands.iter_mut().zip(Command::read(data)).for_each(|(c, new)| *c = new);
    *commanded = true;
}

enum Drives {
    MadeUp { joints: usize },
    Damiao(Damiao),
}

impl Drives {
    fn joints(&self) -> usize {
        match self {
            Drives::MadeUp { joints } => *joints,
            Drives::Damiao(d) => d.motors.len(),
        }
    }

    fn fd(&self) -> bool {
        matches!(self, Drives::Damiao(d) if d.fd)
    }

    fn start(&mut self, bus: &Bus, interface: &str) -> io::Result<()> {
        match self {
            Drives::MadeUp { .. } => Ok(()),
            Drives::Damiao(d) => d.start(bus, interface),
        }
    }

    fn stop(&self, bus: &Bus) -> io::Result<()> {
        match self {
            Drives::MadeUp { .. } => Ok(()),
            Drives::Damiao(d) => d.stop(bus),
        }
    }

    /// A frame from the bus: the joint it reported, if it did.
    fn absorb(&mut self, frame: &Frame, states: &mut [State]) -> Option<usize> {
        match self {
            Drives::MadeUp { joints } => {
                let (joint, state) = can::as_state(frame, *joints)?;
                states[joint] = state;
                Some(joint)
            }
            Drives::Damiao(d) => d.absorb(frame, states),
        }
    }

    /// The moment a state describes: when it was read from drives that
    /// report on their own; when the round that brought the answers went
    /// out, from drives that answer.
    fn stamp(&self, read_ns: u64, round_ns: u64) -> u64 {
        match self {
            Drives::MadeUp { .. } => read_ns,
            Drives::Damiao(_) => round_ns,
        }
    }

    /// A cycle's round of frames.
    fn send(&self, bus: &Bus, commands: &[Command], commanded: bool, states: &[State]) -> io::Result<()> {
        match self {
            Drives::MadeUp { .. } => {
                for (joint, (command, state)) in commands.iter().zip(states).enumerate() {
                    bus.send(&can::command_frame(joint, command.torque(*state)))?;
                }
                Ok(())
            }
            Drives::Damiao(d) => d.send(bus, commands, commanded),
        }
    }
}

struct Damiao {
    motors: Vec<Motor>,
    fd: bool,
}

struct Motor {
    id: u32,
    /// The id it answers on.
    master: u32,
    limits: Limits,
    status: Option<u8>,
}

impl Damiao {
    /// `0x01:0x11,0x02:0x12`: each motor's id and the id it answers on.
    fn from_args(motors: &str, fd: bool) -> io::Result<Self> {
        let id = |s: &str| {
            let s = s.trim();
            match s.strip_prefix("0x") {
                Some(hex) => u32::from_str_radix(hex, 16),
                None => s.parse(),
            }
            .map_err(|_| io::Error::other(format!("--motors: {s:?} isn't an id")))
        };
        let motors = motors
            .split(',')
            .filter(|m| !m.trim().is_empty())
            .map(|m| {
                let (a, b) =
                    m.split_once(':').ok_or_else(|| io::Error::other(format!("--motors: {m:?} isn't id:master_id")))?;
                Ok(Motor { id: id(a)?, master: id(b)?, limits: Limits { p: 1.0, v: 1.0, t: 1.0 }, status: None })
            })
            .collect::<io::Result<Vec<_>>>()?;
        if motors.is_empty() {
            return Err(io::Error::other("--protocol damiao needs --motors, as id:master_id,…"));
        }
        Ok(Self { motors, fd })
    }

    fn frame(&self, id: u32, data: [u8; 8]) -> Frame {
        Frame::new(id, &data).fd(self.fd)
    }

    /// Each motor in MIT mode, its ranges read, then enabled: one request at
    /// a time, nothing else on the bus.
    fn start(&mut self, bus: &Bus, interface: &str) -> io::Result<()> {
        for i in 0..self.motors.len() {
            let id = self.motors[i].id;
            let mode =
                self.ask(bus, interface, i, damiao::write_register(id, reg::CTRL_MODE, reg::MIT_MODE.to_le_bytes()))?;
            if u32::from_le_bytes(mode) != reg::MIT_MODE {
                return Err(io::Error::other(format!(
                    "motor {id:#04x} stays in mode {}: keel speaks MIT mode ({})",
                    u32::from_le_bytes(mode),
                    reg::MIT_MODE
                )));
            }
            let range = |rid| -> io::Result<f64> {
                Ok(f32::from_le_bytes(self.ask(bus, interface, i, damiao::read_register(id, rid))?) as f64)
            };
            let limits = Limits { p: range(reg::PMAX)?, v: range(reg::VMAX)?, t: range(reg::TMAX)? };
            self.motors[i].limits = limits;
            println!(
                "motor {id:#04x}: MIT mode, ±{} rad, ±{} rad/s, ±{} N m, answering on {:#04x}",
                limits.p, limits.v, limits.t, self.motors[i].master
            );
        }
        for m in &self.motors {
            bus.send(&self.frame(m.id, damiao::special(damiao::ENABLE)))?;
        }
        Ok(())
    }

    /// Sends a register request for motor `i`, and waits for its answer.
    fn ask(&self, bus: &Bus, interface: &str, i: usize, request: [u8; 8]) -> io::Result<[u8; 4]> {
        let m = &self.motors[i];
        bus.send(&self.frame(damiao::REGISTER_ID, request))?;
        let until = Instant::now() + REGISTER_TIMEOUT;
        while let Some(left) = until.checked_duration_since(Instant::now()) {
            if !bus.wait(left)? {
                continue;
            }
            while let Some(frame) = bus.try_recv()? {
                let answer = frame.eight().filter(|_| frame.id == m.master).and_then(|d| damiao::register_answer(&d));
                if let Some((id, rid, value)) = answer {
                    if id == m.id && rid == request[3] {
                        return Ok(value);
                    }
                }
            }
        }
        Err(io::Error::other(format!(
            "motor {:#04x} doesn't answer on `{interface}` (its answers should come on {:#04x}{})",
            m.id,
            m.master,
            if self.fd { ", in CAN FD" } else { "" }
        )))
    }

    fn stop(&self, bus: &Bus) -> io::Result<()> {
        for m in &self.motors {
            bus.send(&self.frame(m.id, damiao::special(damiao::DISABLE)))?;
        }
        Ok(())
    }

    fn absorb(&mut self, frame: &Frame, states: &mut [State]) -> Option<usize> {
        let i = self.motors.iter().position(|m| m.master == frame.id)?;
        let m = &mut self.motors[i];
        let f = damiao::feedback(&frame.eight()?, m.limits);
        if m.status != Some(f.status) {
            if f.status >= damiao::ERRORS || m.status.is_some() {
                println!("motor {:#04x}: {} ({:X})", m.id, damiao::status_name(f.status), f.status);
            }
            m.status = Some(f.status);
        }
        states[i] = f.state;
        Some(i)
    }

    /// The commands, or, before there's been one, a request for feedback, so
    /// that states come in from the start.
    fn send(&self, bus: &Bus, commands: &[Command], commanded: bool) -> io::Result<()> {
        for (m, command) in self.motors.iter().zip(commands) {
            let frame = match commanded {
                true => self.frame(m.id, damiao::command(command, m.limits)),
                false => self.frame(damiao::REGISTER_ID, damiao::refresh(m.id)),
            };
            bus.send(&frame)?;
        }
        Ok(())
    }
}
