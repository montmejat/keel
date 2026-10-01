//! `keel fleet`: several robots running one dataflow (see
//! `keel_daemon::fleet`). Each robot is a deployment of its own with its own
//! coordinator; these commands act on several, or on one before the others.

use std::error::Error;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Command, Stdio};

use keel_daemon::control::{Client, NodeState};
use keel_daemon::fleet::{code, Fleet};
use keel_daemon::packaging::{Deployment, Registry};
use keel_daemon::runtime;

use crate::fmt;

type Result<T> = std::result::Result<T, Box<dyn Error>>;

/// Deploys to each robot, then runs them all, each under its own
/// coordinator, until they've all stopped.
pub fn run(path: &Path, only: &[String]) -> Result<bool> {
    let fleet = Fleet::load(path)?;
    let keel = std::env::current_exe()?;
    // Ctrl-C reaches the coordinators too: they stop their robots, we wait.
    keel_daemon::keep_running_on_interrupt();
    let mut robots = Vec::new();
    for robot in fleet.select(only)? {
        let deployment = fleet.deploy(&robot)?;
        let mut child = Command::new(&keel)
            .args(["start", &deployment.id])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let outputs: [Box<dyn Read + Send>; 2] =
            [Box::new(child.stdout.take().unwrap()), Box::new(child.stderr.take().unwrap())];
        for output in outputs {
            let robot = robot.clone();
            std::thread::spawn(move || {
                BufReader::new(output).lines().map_while(|l| l.ok()).for_each(|line| println!("[{robot}] {line}"));
            });
        }
        robots.push((robot, child));
    }
    let mut ok = true;
    for (robot, mut child) in robots {
        let status = child.wait()?;
        println!("[{robot}] {}", if status.success() { "finished".into() } else { format!("failed: {status}") });
        ok &= status.success();
    }
    Ok(ok)
}

/// The coordinator running `name`, if any: its pid and what it's running.
fn running(registry: &Registry, name: &str) -> Option<(u32, Deployment)> {
    runtime::running_daemons().into_iter().find_map(|pid| {
        let status = Client::connect(pid).and_then(|mut c| c.status()).ok()?;
        let deployment = registry.find(status.deployment.as_deref()?).ok()?;
        (status.coordinator && deployment.name == name).then_some((pid, deployment))
    })
}

/// One line per robot: what it runs, and how it's doing.
pub fn status(path: &Path) -> Result<()> {
    let fleet = Fleet::load(path)?;
    let registry = Registry::open()?;
    println!(
        "{:<12} {:<9} {:<9} {:<13} {:>10} {:>6} {:>9} {:>10}",
        "ROBOT", "STATE", "CODE", "DEPLOYMENT", "DEPLOYED", "NODES", "RESTARTS", "LAT p99"
    );
    for robot in fleet.robots.keys() {
        let name = fleet.name(robot);
        let Some((pid, deployment)) = running(&registry, &name) else {
            match registry.current(&name) {
                Ok(d) => {
                    println!("{robot:<12} {:<9} {:<9} {:<13} {:>10}", "stopped", code(&d), d.id, fmt::age(d.created))
                }
                Err(_) => println!("{robot:<12} never deployed"),
            }
            continue;
        };
        let mut client = Client::connect(pid)?;
        let status = client.status()?;
        let up = status.nodes.iter().filter(|n| n.state == NodeState::Running).count();
        let restarts: u32 = status.nodes.iter().map(|n| n.restarts).sum();
        // The slowest input's 99th percentile: one number to compare robots by.
        let worst = client.trace().ok().and_then(|t| t.inputs.iter().map(|i| i.latency.p99).max());
        println!(
            "{robot:<12} {:<9} {:<9} {:<13} {:>10} {:>6} {:>9} {:>10}",
            if status.stopping { "stopping" } else { "running" },
            code(&deployment),
            deployment.id,
            fmt::age(deployment.created),
            format!("{up}/{}", status.nodes.len()),
            restarts,
            worst.map_or("-".into(), fmt::nanos),
        );
    }
    Ok(())
}

/// Rolls `new` into the coordinator at `pid`, which runs `old`.
fn roll(robot: &str, pid: u32, old: &Deployment, new: &Deployment) -> Result<()> {
    if new.id == old.id {
        println!("[{robot}] already runs {} (code {})", new.id, code(new));
        return Ok(());
    }
    if serde_json::to_string(&old.dataflow)? != serde_json::to_string(&new.dataflow)? {
        return Err(format!("[{robot}] the dataflow itself changed: stop the fleet and run it again").into());
    }
    let replaced = Client::connect(pid)?.update(Some(new.id.clone()), new.binaries.clone())?;
    let what =
        if replaced.is_empty() { "no node changed".into() } else { format!("replacing {}", replaced.join(", ")) };
    println!("[{robot}] code {} → {}: {what}", code(old), code(new));
    Ok(())
}

/// Builds the dataflow as it is now and rolls it into the robots (all, or
/// `only` some: try it on one first), one robot after the other, without
/// stopping them.
pub fn update(path: &Path, only: &[String]) -> Result<()> {
    let fleet = Fleet::load(path)?;
    let registry = Registry::open()?;
    for robot in fleet.select(only)? {
        let running = running(&registry, &fleet.name(&robot));
        let new = fleet.deploy(&robot)?;
        match running {
            Some((pid, old)) => roll(&robot, pid, &old, &new)?,
            None => println!("[{robot}] not running: deployed {} (code {}) for its next start", new.id, code(&new)),
        }
    }
    Ok(())
}

/// Puts robots back on the deployment before their current one, running or
/// not.
pub fn rollback(path: &Path, only: &[String]) -> Result<()> {
    let fleet = Fleet::load(path)?;
    let registry = Registry::open()?;
    for robot in fleet.select(only)? {
        let name = fleet.name(&robot);
        let running = running(&registry, &name);
        let previous = registry.rollback(&name, None)?;
        match running {
            Some((pid, old)) => roll(&robot, pid, &old, &previous)?,
            None => println!("[{robot}] not running: back to {} (code {})", previous.id, code(&previous)),
        }
    }
    Ok(())
}
