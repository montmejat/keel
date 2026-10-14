//! Watches an arm and says, in its log, what is wrong with it: a robot's
//! diagnostics, from what the robot publishes, the same whether it's real or
//! simulated. Takes `state`, `command` (position targets), and optionally
//! `frames` (a colour camera) and `imu` (acceleration, then rate of turn);
//! publishes `health`, a line of text: `ok`, or the problems, `; `-separated.
//!
//! What it checks:
//!
//! - **encoders that stop**: a joint whose reading doesn't change at all,
//!   not by a bit, for `--for-s`, while its target moves. A working
//!   encoder's reading always trembles, even on a joint held still, so the
//!   encoder is the suspect, whether the joint follows or not (its drive
//!   may close its loop on an encoder of its own, and keep going).
//! - **joints that don't follow**: a joint more than `--max-error` from its
//!   target for `--for-s`: blocked, or its drive is weak or dead. It's said
//!   to follow again once it has followed for as long, moving: a joint stuck
//!   where its target happens to pass doesn't count.
//! - **states**: how many a second against the start, and how old the
//!   latest is.
//! - **the camera**: the picture frozen (the same pixels again and again
//!   while the arm moves) or dark (much darker than at the start).
//! - **the IMU**: frozen (exactly the same reading again and again), or
//!   much noisier than at the start.
//!
//! Durations are in the messages' own time (their stamps), so the checks
//! hold in lockstep too; only silences are measured on the wall clock.
//! A problem is said once when it starts and once when it ends, and a
//! summary every `--report-s` of wall time.
//!
//! `--max-error 0.2 --for-s 1.0 --report-s 10`

use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

use keel::{Event, Node};
use keel_control::{arg, vector, Command, Image, Mode, State};

const SECOND: u64 = 1_000_000_000;

/// A joint's recent history, to tell what kind of not-following it is.
#[derive(Default)]
struct Joint {
    /// Since when (stamp) it has been too far from its target; and since
    /// when it has followed again, with how far it went meanwhile.
    off_since: Option<u64>,
    on_since: Option<(u64, f64, f64)>,
    /// Its last reading, since when it has been exactly that, and its
    /// target then.
    reading: f64,
    same_since: Option<(u64, f64)>,
    worst: f64,
}

#[derive(Default)]
struct Camera {
    last_hash: u64,
    same_since: Option<u64>,
    baseline: Option<f64>,
    brightness: f64,
    first: Option<u64>,
    seen: Option<Instant>,
}

#[derive(Default)]
struct Imu {
    last: Vec<f64>,
    same_since: Option<u64>,
    /// How much the rate of turn jumps between readings: on average lately,
    /// and at the start.
    jitter: VecDeque<f64>,
    baseline: Option<f64>,
    first: Option<u64>,
}

fn main() -> std::io::Result<()> {
    let max_error: f64 = arg("max-error", 0.2)?;
    let hold = (arg("for-s", 1.0)? * 1e9) as u64;
    let report = Duration::from_secs_f64(arg("report-s", 10.0)?);
    let mut node = Node::from_env()?;

    let mut joints: Vec<Joint> = Vec::new();
    let mut targets: Vec<f64> = Vec::new();
    let mut moving_since: Option<u64> = None;
    let mut state_times: VecDeque<Instant> = VecDeque::new();
    let (mut last_state, mut state_rate) = (None::<Instant>, None::<f64>);
    let mut camera = Camera::default();
    let mut imu = Imu::default();
    let started = Instant::now();
    let mut last_report = Instant::now();
    // What's wrong now, by kind; and what was said.
    let mut problems: BTreeMap<String, String> = BTreeMap::new();
    let mut said: BTreeMap<String, String> = BTreeMap::new();
    let (mut published, mut published_at) = (String::new(), Instant::now());

    loop {
        // Woken at least every 200 ms, to notice what stops arriving.
        let event = node.next_event_timeout(Duration::from_millis(200))?;
        let now = Instant::now();
        match event {
            Some(Event::Stop) => break,
            None => {}
            Some(Event::Input { id, data }) => {
                let t = data.stamp_ns();
                match id {
                    "command" => {
                        targets.clear();
                        targets.extend(Command::read(&data).map(|c| {
                            if c.mode == Mode::Position {
                                c.value
                            } else {
                                f64::NAN
                            }
                        }));
                    }
                    "state" => {
                        let states: Vec<State> = State::read(&data).collect();
                        joints.resize_with(states.len(), Joint::default);
                        state_times.push_back(now);
                        last_state = Some(now);
                        let moving = states.iter().any(|s| s.velocity.abs() > 0.05);
                        moving_since = if moving { moving_since.or(Some(t)) } else { None };
                        for (j, (joint, state)) in joints.iter_mut().zip(&states).enumerate() {
                            let Some(&target) = targets.get(j).filter(|t| t.is_finite()) else { continue };
                            let name = format!("joint{}", j + 1);
                            // The encoder: exactly the same reading while the target moves.
                            let encoder = format!("{name} encoder");
                            if state.position != joint.reading {
                                (joint.reading, joint.same_since) = (state.position, None);
                                problems.remove(&encoder);
                            }
                            let (same, then) = *joint.same_since.get_or_insert((t, target));
                            if t - same >= hold && (target - then).abs() > 0.02 {
                                let what = format!(
                                    "{name} reads exactly {:.4}, not a bit of change, for {:.1} s, while its target went from {then:.2} to {target:.2}: its position sensor (encoder) has stopped updating",
                                    state.position,
                                    (t - same) as f64 / 1e9
                                );
                                problems.entry(encoder.clone()).or_insert(what);
                            }
                            // Following: too far for too long, and back for as long.
                            let error = (target - state.position).abs();
                            joint.worst = joint.worst.max(error);
                            if error <= max_error {
                                joint.off_since = None;
                                let p = state.position;
                                let (back, lo, hi) = joint.on_since.get_or_insert((t, p, p));
                                (*lo, *hi) = (lo.min(p), hi.max(p));
                                if t - *back >= hold && *hi - *lo > max_error {
                                    problems.remove(&name);
                                }
                                continue;
                            }
                            joint.on_since = None;
                            let since = *joint.off_since.get_or_insert(t);
                            if t - since >= hold && !problems.contains_key(&encoder) {
                                let what = format!(
                                    "{name} isn't reaching its target: {error:.2} rad away for {:.1} s, at {:.2}, asked for {target:.2}: blocked, or its drive is weak or dead",
                                    (t - since) as f64 / 1e9,
                                    state.position
                                );
                                problems.entry(name).or_insert(what);
                            }
                        }
                    }
                    "frames" => {
                        let Some((width, height, _, pixels)) = Image::read(&data) else { continue };
                        camera.seen = Some(now);
                        let first = *camera.first.get_or_insert(t);
                        // A sample of the pixels: enough to tell a frozen picture and its brightness.
                        let (mut hash, mut sum, mut n) = (0xcbf29ce484222325u64, 0u64, 0u64);
                        for (i, &p) in pixels.iter().enumerate().step_by(97) {
                            hash = (hash ^ p as u64).wrapping_mul(0x100000001b3) ^ i as u64;
                            sum += p as u64;
                            n += 1;
                        }
                        camera.brightness = sum as f64 / n.max(1) as f64;
                        if camera.baseline.is_none() && t - first > 2 * SECOND {
                            camera.baseline = Some(camera.brightness);
                            println!("camera: {width}x{height}, brightness {:.0} to compare with", camera.brightness);
                        }
                        camera.same_since = if hash == camera.last_hash { camera.same_since.or(Some(t)) } else { None };
                        camera.last_hash = hash;
                        // A picture can't stay the same while the arm moves in front of it.
                        match (camera.same_since, moving_since) {
                            (Some(same), Some(moving)) if t - same > 2 * SECOND && t - moving > SECOND => {
                                let what = format!(
                                    "camera picture frozen: the same pixels for {:.1} s while the arm moves",
                                    (t - same) as f64 / 1e9
                                );
                                problems.entry("frozen".into()).or_insert(what);
                            }
                            (None, _) => drop(problems.remove("frozen")),
                            _ => {}
                        }
                        match camera.baseline {
                            Some(base) if camera.brightness < base * 0.4 => {
                                let what = format!(
                                    "camera picture dark: brightness {:.0}, against {base:.0} at the start: lens covered, or the light is gone",
                                    camera.brightness
                                );
                                problems.entry("dark".into()).or_insert(what);
                            }
                            _ => drop(problems.remove("dark")),
                        }
                    }
                    "imu" => {
                        let values: Vec<f64> = vector::read(&data).collect();
                        let first = *imu.first.get_or_insert(t);
                        if values == imu.last {
                            let same = *imu.same_since.get_or_insert(t);
                            if t - same > SECOND / 2 {
                                let what = format!(
                                    "IMU frozen: exactly the same reading for {:.1} s: the sensor or its link has stopped",
                                    (t - same) as f64 / 1e9
                                );
                                problems.entry("imu frozen".into()).or_insert(what);
                            }
                        } else {
                            imu.same_since = None;
                            problems.remove("imu frozen");
                        }
                        // The rate of turn's jumps between readings: noise, mostly.
                        if values.len() >= 6 && imu.last.len() >= 6 {
                            let jump = (3..6).map(|k| (values[k] - imu.last[k]).abs()).sum::<f64>() / 3.0;
                            imu.jitter.push_back(jump);
                            if imu.jitter.len() > 200 {
                                imu.jitter.pop_front();
                            }
                            let mean = imu.jitter.iter().sum::<f64>() / imu.jitter.len() as f64;
                            if imu.baseline.is_none() && t - first > 2 * SECOND {
                                imu.baseline = Some(mean);
                                println!("imu: jitter {mean:.4} rad/s to compare with");
                            }
                            match imu.baseline {
                                Some(base) if mean > base * 5.0 && mean > 1e-6 => {
                                    let what = format!(
                                        "IMU noisy: its rate of turn jumps {mean:.3} rad/s between readings, against {base:.3} at the start: loose, vibrating, or failing"
                                    );
                                    problems.entry("imu noisy".into()).or_insert(what);
                                }
                                _ => drop(problems.remove("imu noisy")),
                            }
                        }
                        imu.last = values;
                    }
                    _ => {}
                }
            }
        }

        // Rates and silences, on the wall clock.
        let window = Duration::from_secs(5);
        state_times.retain(|t| now - *t < window);
        let rate = state_times.len() as f64 / window.as_secs_f64();
        if state_rate.is_none() && now - started > Duration::from_secs(8) && !state_times.is_empty() {
            state_rate = Some(rate);
        }
        match (state_rate, last_state) {
            (Some(usual), Some(last)) if rate < usual * 0.6 && now - last <= Duration::from_secs(1) => {
                let what = format!("states arriving slowly: {rate:.0} a second, against {usual:.0} at the start");
                problems.entry("slow".into()).or_insert(what);
            }
            _ => drop(problems.remove("slow")),
        }
        match last_state {
            Some(last) if now - last > Duration::from_secs(1) => {
                let what = format!("no state from the arm for {:.1} s", (now - last).as_secs_f64());
                problems.insert("no state".into(), what);
            }
            _ => drop(problems.remove("no state")),
        }
        match camera.seen {
            Some(seen) if now - seen > Duration::from_secs(2) => {
                let what = format!("no camera frame for {:.1} s", (now - seen).as_secs_f64());
                problems.insert("no frame".into(), what);
            }
            _ => drop(problems.remove("no frame")),
        }

        for (kind, what) in &problems {
            if !said.contains_key(kind) {
                println!("PROBLEM {what}");
            }
        }
        for kind in said.keys() {
            if !problems.contains_key(kind) {
                println!("cleared: {kind}");
            }
        }
        said = problems.clone();

        if now - last_report >= report {
            last_report = now;
            let worst = (joints.iter().enumerate())
                .max_by(|a, b| a.1.worst.total_cmp(&b.1.worst))
                .map(|(j, joint)| format!("worst tracking joint{} {:.3}", j + 1, joint.worst))
                .unwrap_or_else(|| "no joints yet".into());
            joints.iter_mut().for_each(|j| j.worst = 0.0);
            let verdict = if problems.is_empty() { "ok".to_string() } else { format!("{} problem(s)", problems.len()) };
            println!("health {verdict}: {rate:.0} states/s, {worst}, camera brightness {:.0}", camera.brightness);
        }
        let line = if problems.is_empty() {
            "ok".to_string()
        } else {
            problems.values().cloned().collect::<Vec<_>>().join("; ")
        };
        if line != published || now - published_at >= Duration::from_secs(1) {
            node.send_output("health", line.as_bytes())?;
            (published, published_at) = (line, now);
        }
    }
    Ok(())
}
