//! A policy, as a node (decision 0036): an ONNX network that turns joint
//! states and an IMU reading into joint targets, run by tract.
//!
//! What the network was trained with is in its file's metadata: the
//! joints it drives and their default pose, the action scale, and the
//! names of its observation's parts, which [`Layout`] reads in order. The
//! parts keel knows are those of Pollen's Microduck policies
//! (`duck-control`'s `obs.rs`): the gyro, gravity in the body's frame, the
//! joints' positions from the default pose, their velocities, the last
//! action, and the commands (walk, head, body). The action becomes targets
//! as Pollen's runtime applies it (`robotd`'s `control.rs`): the default
//! pose plus the scale times the action, through a low-pass filter.

use std::io;
use std::path::Path;

use keel_control::{Command, Imu, State};
use tract_onnx::prelude::*;

/// A part of an observation: its name in the network's metadata, and how
/// many numbers it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    /// `base_ang_vel`: the gyro, rad/s, in the body's frame.
    Gyro,
    /// `projected_gravity`: which way is down, in the body's frame.
    Gravity,
    /// `joint_pos`: each joint's position, less its default pose.
    Positions,
    /// `joint_vel`: each joint's velocity.
    Velocities,
    /// `actions`: the network's last output.
    LastAction,
    /// `command`: walking forward, left, and turning (m/s, m/s, rad/s).
    Walk,
    /// `head_command`: neck pitch, head pitch, yaw and roll, from the
    /// default pose.
    Head,
    /// `body_command`: the body's x, y, z, roll, pitch, yaw.
    Body,
}

impl Part {
    fn named(name: &str) -> Option<Part> {
        Some(match name {
            "base_ang_vel" => Part::Gyro,
            "projected_gravity" => Part::Gravity,
            "joint_pos" => Part::Positions,
            "joint_vel" => Part::Velocities,
            "actions" => Part::LastAction,
            "command" => Part::Walk,
            "head_command" => Part::Head,
            "body_command" => Part::Body,
            _ => return None,
        })
    }

    fn width(self, joints: usize) -> usize {
        match self {
            Part::Gyro | Part::Gravity | Part::Walk => 3,
            Part::Positions | Part::Velocities | Part::LastAction => joints,
            Part::Head => 4,
            Part::Body => 6,
        }
    }
}

/// The observation's parts, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout(pub Vec<Part>);

impl Layout {
    /// From the metadata's `observation_names`, for a network of `joints`
    /// joints taking `width` numbers: every name known, the widths adding up.
    pub fn new(names: &[String], joints: usize, width: usize) -> io::Result<Layout> {
        let parts: Vec<Part> = names
            .iter()
            .map(|n| {
                Part::named(n)
                    .ok_or_else(|| io::Error::other(format!("the policy observes `{n}`, which keel doesn't know")))
            })
            .collect::<io::Result<_>>()?;
        let total: usize = parts.iter().map(|p| p.width(joints)).sum();
        if total != width {
            return Err(io::Error::other(format!(
                "the policy's observation is {width} numbers, but its parts ({}) make {total}",
                names.join(", ")
            )));
        }
        Ok(Layout(parts))
    }
}

/// What the policy is asked to do.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Commands {
    pub walk: [f64; 3],
    pub head: [f64; 4],
    pub body: [f64; 6],
}

/// A network, and what it was trained with.
pub struct Policy {
    plan: TypedSimpleState,
    pub layout: Layout,
    /// The joints it drives, in its order, and their default pose.
    pub joints: Vec<String>,
    pub default: Vec<f64>,
    pub action_scale: f64,
    width: usize,
}

/// A metadata entry, as tract keeps it.
fn property(properties: &std::collections::HashMap<String, Arc<Tensor>>, key: &str) -> io::Result<String> {
    let value = properties
        .get(&format!("onnx.metadata_props.{key}"))
        .ok_or_else(|| io::Error::other(format!("the policy's metadata has no `{key}`")))?;
    value.try_as_plain_ram().and_then(|t| t.to_scalar::<String>().cloned()).map_err(|e| io::Error::other(e.to_string()))
}

/// `a, b, c` or `[a, b, c]`, as the metadata writes lists.
fn list(text: &str) -> Vec<String> {
    text.trim_matches(|c| c == '[' || c == ']')
        .split(',')
        .map(|s| s.trim().trim_matches(|c| c == '"' || c == '\'').to_owned())
        .filter(|s| !s.is_empty())
        .collect()
}

fn numbers(text: &str, key: &str) -> io::Result<Vec<f64>> {
    list(text)
        .iter()
        .map(|s| s.parse().map_err(|_| io::Error::other(format!("`{key}`: {s:?} isn't a number"))))
        .collect()
}

impl Policy {
    pub fn load(path: &Path) -> io::Result<Policy> {
        let err = |e: TractError| io::Error::other(format!("{}: {e}", path.display()));
        let model = tract_onnx::onnx().model_for_path(path).map_err(err)?;
        let property = |key: &str| property(&model.properties, key);
        let joints = list(&property("joint_names")?);
        let default = numbers(&property("default_joint_pos")?, "default_joint_pos")?;
        if joints.len() != default.len() {
            return Err(io::Error::other("the policy's joint_names and default_joint_pos differ in length"));
        }
        let action_scale = property("action_scale")?.trim().parse().unwrap_or(1.0);
        let names = list(&property("observation_names")?);
        // The parts' widths make the input's; tract checks it against the
        // network's own as it optimizes it.
        let width = names.iter().filter_map(|n| Part::named(n)).map(|p| p.width(joints.len())).sum();
        let layout = Layout::new(&names, joints.len(), width)?;
        let plan = model
            .with_input_fact(0, f32::fact([1, width]).into())
            .and_then(|m| m.into_optimized())
            .and_then(|m| m.into_runnable())
            .and_then(|m| m.spawn())
            .map_err(|e| io::Error::other(format!("{}: as {width} numbers in ({e})", path.display())))?;
        Ok(Policy { plan, layout, joints, default, action_scale, width })
    }

    /// The observation, in the network's order: `positions` and
    /// `velocities` in its joints' order.
    pub fn observe(&self, imu: &Imu, positions: &[f64], velocities: &[f64], last: &[f32], c: &Commands) -> Vec<f32> {
        let mut obs: Vec<f64> = Vec::with_capacity(self.width);
        for part in &self.layout.0 {
            match part {
                Part::Gyro => obs.extend(imu.angular_velocity),
                Part::Gravity => obs.extend(imu.gravity()),
                Part::Positions => obs.extend(positions.iter().zip(&self.default).map(|(p, d)| p - d)),
                Part::Velocities => obs.extend(velocities),
                Part::LastAction => obs.extend(last.iter().map(|&a| a as f64)),
                Part::Walk => obs.extend(c.walk),
                Part::Head => obs.extend(c.head),
                Part::Body => obs.extend(c.body),
            }
        }
        obs.into_iter().map(|x| x as f32).collect()
    }

    /// The network's output for `observation`.
    pub fn act(&mut self, observation: &[f32]) -> io::Result<Vec<f32>> {
        let err = |e: TractError| io::Error::other(e.to_string());
        let input = Tensor::from_shape(&[1, self.width], observation).map_err(err)?;
        let out = self.plan.run(tvec!(input.into())).map_err(err)?;
        Ok(out[0].try_as_plain_ram().map_err(err)?.as_slice::<f32>().map_err(err)?.to_vec())
    }
}

/// From actions to joint targets, as Pollen's runtime applies them: the
/// default pose plus `scale` times the action, then each joint pulled only
/// `alpha` of the way from its last target (a first-order low-pass; 1 for
/// none).
pub struct Targets {
    pub scale: f64,
    pub alpha: Vec<f64>,
    last: Option<Vec<f64>>,
}

impl Targets {
    pub fn new(scale: f64, alpha: Vec<f64>) -> Targets {
        Targets { scale, alpha, last: None }
    }

    pub fn next(&mut self, default: &[f64], action: &[f32]) -> Vec<f64> {
        let mut targets: Vec<f64> = default.iter().zip(action).map(|(d, &a)| d + self.scale * a as f64).collect();
        if let Some(last) = &self.last {
            for ((t, l), a) in targets.iter_mut().zip(last).zip(&self.alpha) {
                *t = a * *t + (1.0 - a) * l;
            }
        }
        self.last = Some(targets.clone());
        targets
    }
}

/// The commands for every joint of a `state`, in its order: a policy's
/// targets where `map` places one, and `hold` for the joints it doesn't
/// drive.
pub fn commands(targets: &[f64], map: &[Option<usize>], hold: &[f64]) -> Vec<Command> {
    map.iter()
        .zip(hold)
        .map(|(m, &h)| Command { position: m.map_or(h, |i| targets[i]), ..Default::default() })
        .collect()
}

/// For each joint of a state, named `names` in its order, the policy's
/// joint it is, if any. Every policy joint must be there.
pub fn mapping(names: &[String], policy: &[String]) -> io::Result<Vec<Option<usize>>> {
    for p in policy {
        if !names.contains(p) {
            return Err(io::Error::other(format!(
                "the policy drives `{p}`, which isn't among --joints ({}): name the state's joints in its order",
                names.join(",")
            )));
        }
    }
    Ok(names.iter().map(|n| policy.iter().position(|p| p == n)).collect())
}

/// The state's joints in the policy's order: their positions and
/// velocities.
pub fn gather(states: &[State], map: &[Option<usize>], n: usize) -> (Vec<f64>, Vec<f64>) {
    let (mut p, mut v) = (vec![0.0; n], vec![0.0; n]);
    for (s, m) in states.iter().zip(map) {
        if let Some(i) = *m {
            (p[i], v[i]) = (s.position, s.velocity);
        }
    }
    (p, v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(s: &[&str]) -> Vec<String> {
        s.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_microducks_layout_is_61_numbers() {
        let parts = names(&[
            "base_ang_vel",
            "projected_gravity",
            "joint_pos",
            "joint_vel",
            "actions",
            "command",
            "head_command",
            "body_command",
        ]);
        assert_eq!(Layout::new(&parts, 14, 61).unwrap().0.len(), 8);
        assert!(Layout::new(&parts, 14, 60).unwrap_err().to_string().contains("make 61"));
        assert!(Layout::new(&names(&["phase"]), 14, 1).unwrap_err().to_string().contains("`phase`"));
    }

    #[test]
    fn targets_are_the_default_pose_plus_the_scaled_action_low_passed() {
        let mut t = Targets::new(0.5, vec![0.7, 1.0]);
        // Actions are f32: equal to their precision.
        let first = t.next(&[1.0, 2.0], &[0.2, -0.2]);
        assert!((first[0] - 1.1).abs() < 1e-6 && (first[1] - 1.9).abs() < 1e-6, "{first:?}");
        let second = t.next(&[1.0, 2.0], &[0.4, -0.4]);
        assert!((second[0] - (0.7 * 1.2 + 0.3 * 1.1)).abs() < 1e-6 && (second[1] - 1.8).abs() < 1e-6, "{second:?}");
    }

    #[test]
    fn joints_map_by_name_and_the_rest_hold() {
        let state = names(&["a", "mouth", "b"]);
        let map = mapping(&state, &names(&["b", "a"])).unwrap();
        assert_eq!(map, [Some(1), None, Some(0)]);
        let c = commands(&[0.2, 0.1], &map, &[9.0, 0.5, 9.0]);
        assert_eq!(c.iter().map(|c| c.position).collect::<Vec<_>>(), [0.1, 0.5, 0.2]);
        assert!(mapping(&names(&["a"]), &names(&["a", "b"])).is_err());
    }

    #[test]
    fn list_reads_the_metadatas_lists() {
        assert_eq!(list("[\"a\", 'b', c]"), ["a", "b", "c"]);
        assert_eq!(numbers("0.5, -1", "x").unwrap(), [0.5, -1.0]);
    }
}
