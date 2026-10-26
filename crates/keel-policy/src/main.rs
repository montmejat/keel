//! A policy as a node (decision 0036): each `state`, with the latest `imu`,
//! through an ONNX network, answered with a `command` (positions).
//!
//! `--model` is the network's file. `--joints` names the state's joints in
//! its order, so the network's (its metadata's `joint_names`) are found by
//! name; the others hold where they were at the first state. `--walk`,
//! `--head` and `--body` are what it's asked to do; a `walk` input, if the
//! dataflow gives it one (`keel-keyboard`), steers it as it runs. Targets are low-passed
//! as Pollen's runtime does: `--lowpass` for every joint, `--lowpass-head`
//! for `--head-joints`.
//!
//! `--model velstand.onnx --joints a,b,c --walk 0,0,0 --head 0,0,0,0 --body 0,0,0,0,0,0 --lowpass 0.7 --lowpass-head 0.5`

use std::io;
use std::path::PathBuf;

use keel::{Event, Node};
use keel_control::{arg, joints, Command, Imu, State, Walk};
use keel_policy::{commands, gather, mapping, Commands, Policy, Targets};

fn main() -> std::process::ExitCode {
    joints::exit_on(main_())
}

fn floats<const N: usize>(name: &str) -> io::Result<[f64; N]> {
    let text: String = arg(name, vec!["0"; N].join(","))?;
    let v: Vec<f64> = text
        .split(',')
        .map(|s| s.trim().parse())
        .collect::<Result<_, _>>()
        .map_err(|_| io::Error::other(format!("--{name} {text:?}: {N} numbers, comma-separated")))?;
    v.try_into().map_err(|_| io::Error::other(format!("--{name}: {N} numbers")))
}

fn names(text: &str) -> Vec<String> {
    text.split(',').map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()).collect()
}

fn main_() -> io::Result<()> {
    let model: PathBuf = arg::<String>("model", String::new())?.into();
    if !model.is_file() {
        return Err(io::Error::other(format!(
            "no policy at `{}`. Pollen's Microduck walking policy, for one:\n\n  \
             curl -fL https://huggingface.co/pollen-robotics/microduck-policies/resolve/main/velstand.onnx -o {}\n",
            model.display(),
            model.display()
        )));
    }
    let mut policy = Policy::load(&model)?;
    let state_joints = names(&arg::<String>("joints", String::new())?);
    let map = mapping(&state_joints, &policy.joints)?;
    let mut asked = Commands { walk: floats("walk")?, head: floats("head")?, body: floats("body")? };
    let head_joints = names(&arg::<String>("head-joints", "neck_pitch,head_pitch,head_yaw,head_roll".into())?);
    let (legs, head): (f64, f64) = (arg("lowpass", 0.7)?, arg("lowpass-head", 0.5)?);
    let alpha = policy.joints.iter().map(|j| if head_joints.contains(j) { head } else { legs }).collect();
    let scale: f64 = arg("action-scale", policy.action_scale)?;
    let mut targets = Targets::new(scale, alpha);
    println!(
        "{}: {} joints, {} numbers in, action scale {scale}",
        model.display(),
        policy.joints.len(),
        policy.layout.0.len()
    );

    let mut node = Node::from_env()?;
    let n = policy.joints.len();
    let (mut imu, mut last, mut hold) = (None::<Imu>, vec![0f32; n], None::<Vec<f64>>);
    let (mut started, mut said, mut waiting_said) = (None, 0u64, false);
    while let Event::Input { id, data } = node.next_event()? {
        if id == "imu" {
            imu = Imu::read(&data);
            continue;
        }
        if id == "walk" {
            if let Some(w) = Walk::read(&data) {
                asked.walk = [w.forward, w.left, w.turn];
            }
            continue;
        }
        let states: Vec<State> = State::read(&data).collect();
        if states.len() != state_joints.len() {
            return Err(io::Error::other(format!(
                "a state of {} joints, but --joints names {}",
                states.len(),
                state_joints.len()
            )));
        }
        let hold = hold.get_or_insert_with(|| states.iter().map(|s| s.position).collect());
        let Some(reading) = imu else {
            if !waiting_said {
                println!("waiting for the IMU: the policy needs it");
                waiting_said = true;
            }
            continue;
        };
        let (positions, velocities) = gather(&states, &map, n);
        let observation = policy.observe(&reading, &positions, &velocities, &last, &asked);
        last = policy.act(&observation)?;
        let goals = targets.next(&policy.default, &last);
        let commands: Vec<Command> = commands(&goals, &map, hold);
        let now = data.stamp_ns();
        let t = (now - *started.get_or_insert(now)) as f64 / 1e9;
        if now.saturating_sub(said) >= 1_000_000_000 {
            said = now;
            let g = reading.gravity();
            let [f, l, w] = asked.walk;
            println!("{t:4.1}s down [{:+.2} {:+.2} {:+.2}]; walking {f:+.2} m/s forward, {l:+.2} left, turning {w:+.1} rad/s", g[0], g[1], g[2]);
        }
        // Sent while `data` is held: the command answers this state.
        node.send_with("command", commands.len() * Command::LEN, |p| Command::write(&commands, p))?;
    }
    Ok(())
}
