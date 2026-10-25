//! Watching joints: what each is told (its `command`) against what it does
//! (its `state`), for the signs of a drive or a sensor gone wrong. The
//! joints' loop runs it on every state it reads: it marks the state's
//! [`Status`](crate::Status), and says in the log what it sees, with the
//! numbers, not what it thinks broke: the reader (a person, an agent)
//! concludes.
//!
//! Three signs, each when it has lasted `hold` seconds:
//!
//! - **gives far less torque than asked**: the command's law, on the joint
//!   as it reads, asks for a torque, and the drive reports giving a fraction
//!   of it. A drive that's dead, weak, or held back by its limits.
//! - **stays far from its target**: a position command the joint doesn't
//!   follow.
//! - **its reading doesn't change while its target moves**: a frozen
//!   sensor; the drive, closing its loop on it, may be pushing hard on a
//!   joint that has moved.
//!
//! Time is the states' stamps, as for a controller: it watches a replay or
//! a simulation in lockstep the same way.

use crate::{Command, State, Status};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Settings {
    /// Farther than this from its target, rad, a joint isn't following.
    pub error: f64,
    /// Giving less than this fraction of the torque asked is too little…
    pub ratio: f64,
    /// …when at least this much is asked, N m.
    pub min_asked: f64,
    /// A sign counts once it has lasted this long, s.
    pub hold: f64,
    /// A target moving at least this much, rad, should move the reading.
    pub moved: f64,
    /// Nothing counts this long after the first command, s: joints settle.
    pub grace: f64,
    /// A summary line this often, s.
    pub summary: f64,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { error: 0.2, ratio: 0.3, min_asked: 1.0, hold: 0.5, moved: 0.05, grace: 2.0, summary: 5.0 }
    }
}

/// The signs, in the order they're told, and their flags in a status.
const SIGNS: usize = 3;
const UNDERPOWERED: usize = 0;
const BEHIND: usize = 1;
const FROZEN: usize = 2;
const FLAGS: [u32; SIGNS] = [Status::UNDERPOWERED, Status::BEHIND, Status::FROZEN];

#[derive(Debug, Clone, Default)]
struct Joint {
    /// When each sign was first seen, in its current run.
    since: [Option<f64>; SIGNS],
    /// Which signs have been told, as lasting.
    told: [bool; SIGNS],
    /// The reading, since when it hasn't changed, the target then, and since
    /// when the target has been `moved` away from it, if it has.
    still: Option<Still>,
}

#[derive(Debug, Clone, Copy)]
struct Still {
    reading: f64,
    since: f64,
    target: f64,
    moved_since: Option<f64>,
}

pub struct Watch {
    settings: Settings,
    names: Vec<String>,
    joints: Vec<Joint>,
    /// When the first command came, and when the last summary was said.
    started: Option<f64>,
    summarised: f64,
}

impl Watch {
    /// `names`: what to call each joint after its number, if anything.
    pub fn new(settings: Settings, names: Vec<String>) -> Self {
        Watch { settings, names, joints: Vec::new(), started: None, summarised: 0.0 }
    }

    fn name(&self, j: usize) -> String {
        match self.names.get(j).filter(|n| !n.is_empty()) {
            Some(n) => format!("joint {} ({n})", j + 1),
            None => format!("joint {}", j + 1),
        }
    }

    /// A state, at stamp `t` seconds, and the newest commands: marks each
    /// joint's status with the signs that have lasted, and gives what's
    /// worth saying, as lines.
    pub fn observe(&mut self, t: f64, states: &mut [State], commands: &[Command]) -> Vec<String> {
        let s = self.settings;
        self.joints.resize(states.len(), Joint::default());
        if commands.is_empty() {
            return Vec::new();
        }
        let started = *self.started.get_or_insert(t);
        let settled = t - started >= s.grace;
        let mut lines = Vec::new();
        for (j, (state, command)) in states.iter_mut().zip(commands).enumerate() {
            let asked = command.torque(*state);
            let gives = state.effort;
            let off = state.position - command.position;
            let joint = &mut self.joints[j];

            // The reading's stillness, and whether the target has moved away
            // meanwhile: a still reading is only a sign once the target has
            // moved and the reading hasn't followed it for a while.
            let target = command.position;
            let mut still = match joint.still {
                Some(still) if still.reading == state.position => still,
                _ => Still { reading: state.position, since: t, target, moved_since: None },
            };
            if still.moved_since.is_none() && (target - still.target).abs() >= s.moved {
                still.moved_since = Some(t);
            }
            joint.still = Some(still);

            let now = [
                asked.abs() >= s.min_asked && gives.abs() < s.ratio * asked.abs(),
                command.kp > 0.0 && off.abs() > s.error,
                still.moved_since.is_some(),
            ];
            let mut changed = false;
            for (k, &seen) in now.iter().enumerate() {
                if !seen {
                    joint.since[k] = None;
                } else if joint.since[k].is_none() {
                    // A still reading counts from when the target moved away.
                    joint.since[k] = Some(if k == FROZEN { still.moved_since.unwrap_or(t) } else { t });
                }
                let lasting = settled && joint.since[k].is_some_and(|since| t - since >= s.hold);
                changed |= lasting != joint.told[k];
                joint.told[k] = lasting;
                if lasting {
                    state.status.flags |= FLAGS[k];
                }
            }
            if !changed {
                continue;
            }
            let told = self.joints[j].told;
            let mut said = Vec::new();
            // No "-0.0".
            let tidy = |x: f64| if x.abs() < 0.05 { 0.0 } else { x };
            if told[UNDERPOWERED] {
                said.push(format!("gives {:.1} N m of the {:.1} asked", tidy(gives), tidy(asked)));
            }
            if told[BEHIND] {
                said.push(format!("{off:+.2} rad from its target ({:.2})", command.position));
            }
            if told[FROZEN] {
                said.push(format!(
                    "its position reads {:.3} rad, unchanged for {:.1} s, while its target went to {:.2}",
                    state.position,
                    t - still.since,
                    target
                ));
            }
            let (name, at) = (self.name(j), t - started);
            lines.push(match said.is_empty() {
                true => format!("{at:.1}s {name}: following its commands again"),
                false => format!("{at:.1}s {name}: {}", said.join("; ")),
            });
        }
        if settled && t - self.summarised >= s.summary {
            self.summarised = t;
            let t = t - started;
            let troubled: Vec<String> = (0..self.joints.len())
                .filter(|&j| self.joints[j].told.iter().any(|&x| x))
                .map(|j| self.name(j))
                .collect();
            lines.push(match troubled.is_empty() {
                true if self.joints.len() == 1 => format!("{t:.1}s the joint follows its commands"),
                true => format!("{t:.1}s all {} joints follow their commands", self.joints.len()),
                false => format!("{t:.1}s not following: {}", troubled.join(", ")),
            });
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hold(p: f64) -> Command {
        Command { position: p, velocity: 0.0, kp: 60.0, kd: 2.0, effort: 0.0 }
    }

    fn state(p: f64, effort: f64) -> State {
        State { position: p, velocity: 0.0, effort, ..Default::default() }
    }

    /// Runs `f(t)` → (state, command) for a joint at 500 Hz until `until` s,
    /// and gives every line said.
    fn run(until: f64, f: impl Fn(f64) -> (State, Command)) -> Vec<String> {
        let mut watch = Watch::new(Settings::default(), vec!["elbow".into()]);
        let mut lines = Vec::new();
        for i in 0..(until * 500.0) as usize {
            let t = i as f64 / 500.0;
            let (s, c) = f(t);
            let mut states = [s];
            lines.extend(watch.observe(t, &mut states, &[c]));
            // The status says what the lines say.
            let told = lines.last().filter(|l| !l.contains("follow")).is_some();
            if told && lines.last().is_some_and(|l| l.contains("elbow")) {
                assert!(!states[0].status.ok(), "{lines:?}");
            }
        }
        lines
    }

    #[test]
    fn a_healthy_joint_says_only_so() {
        // Near its target, giving what its law asks.
        let lines = run(6.0, |_| {
            let (s, c) = (state(1.55, 0.0), hold(1.57));
            (State { effort: c.torque(s), ..s }, c)
        });
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("the joint follows its commands"), "{lines:?}");
    }

    #[test]
    fn a_dead_motor_gives_nothing_and_falls_behind() {
        // Fine for 3 s, then no torque, and the joint sags to its stop.
        let lines = run(5.0, |t| {
            let c = hold(1.57);
            if t < 3.0 {
                let s = state(1.55, 0.0);
                (State { effort: c.torque(s), ..s }, c)
            } else {
                (state(2.44, 0.0), c)
            }
        });
        let said = lines.iter().find(|l| l.contains("elbow") && l.contains("gives")).expect("said");
        assert!(said.contains("gives 0.0 N m") && said.contains("+0.87 rad from its target"), "{said}");
        assert!(said.starts_with("3.5s"), "after holding half a second: {said}");
        assert!(lines.iter().all(|l| !l.contains("-0.0")), "{lines:?}");
    }

    #[test]
    fn a_frozen_reading_is_told_when_the_target_moves() {
        // The target swings; the reading stops at 2 s.
        let lines = run(4.0, |t| {
            let c = hold(0.4 * (std::f64::consts::TAU * 0.25 * t).sin());
            let p = if t < 2.0 { c.position } else { 0.4 };
            (State { effort: c.torque(state(p, 0.0)), ..state(p, 0.0) }, c)
        });
        let said = lines.iter().find(|l| l.contains("unchanged")).expect("said");
        assert!(said.contains("reads 0.400 rad"), "{said}");
    }

    #[test]
    fn a_target_that_jumps_gives_the_reading_time_to_follow() {
        // A gripper, still at 0 for 3 s, told to close: it starts moving
        // 0.1 s later, as a real one does.
        let lines = run(5.0, |t| {
            let c = hold(if t < 3.0 { 0.0 } else { -1.0 });
            let p = if t < 3.1 { 0.0 } else { (-(t - 3.1) * 4.0).max(-1.0) };
            (State { effort: c.torque(state(p, 0.0)), ..state(p, 0.0) }, c)
        });
        assert!(lines.iter().all(|l| !l.contains("unchanged")), "{lines:?}");
    }

    #[test]
    fn nothing_counts_while_joints_settle() {
        // Far from target for the first second only.
        let lines = run(4.0, |t| {
            let c = hold(1.57);
            let s = state(if t < 1.0 { 2.44 } else { 1.55 }, 0.0);
            (State { effort: c.torque(s), ..s }, c)
        });
        assert!(lines.iter().all(|l| !l.contains("rad from")), "{lines:?}");
    }

    #[test]
    fn a_joint_that_recovers_says_so() {
        let lines = run(6.0, |t| {
            let c = hold(1.57);
            let s = state(if (3.0..4.0).contains(&t) { 2.0 } else { 1.55 }, 0.0);
            (State { effort: c.torque(s), ..s }, c)
        });
        assert!(lines.iter().any(|l| l.contains("following its commands again")), "{lines:?}");
    }
}
