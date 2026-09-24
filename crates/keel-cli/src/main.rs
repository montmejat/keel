//! `keel`: run dataflows and inspect the running ones.
//!
//! Every command except `run` is a client of a daemon's control API.

mod fmt;
mod top;

use std::error::Error;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use keel_daemon::control::{Client, LogLine, NodeState};
use keel_daemon::runtime;

#[derive(Parser)]
#[command(name = "keel", version, about = "A minimal robotics-style middleware")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run a dataflow on this machine, in the foreground
    Run { dataflow: PathBuf },
    /// List the running dataflows
    Ps,
    /// Live view of a running dataflow
    Top {
        /// Daemon to watch; needed only when several are running
        #[arg(long)]
        pid: Option<u32>,
    },
    /// Print the logs of a running dataflow
    Logs {
        /// Only this node's lines
        node: Option<String>,
        /// Keep printing new lines until the dataflow exits
        #[arg(short, long)]
        follow: bool,
        #[arg(long)]
        pid: Option<u32>,
    },
    /// Stop a running dataflow gracefully
    Stop {
        #[arg(long)]
        pid: Option<u32>,
    },
}

fn main() -> ExitCode {
    match run(Cli::parse().command) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("keel: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(command: Command) -> Result<ExitCode, Box<dyn Error>> {
    match command {
        Command::Run { dataflow } => {
            let ok = keel_daemon::run(&dataflow)?;
            return Ok(if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE });
        }
        Command::Ps => ps()?,
        Command::Top { pid } => top::run(pick(pid)?)?,
        Command::Logs { node, follow, pid } => logs(pick(pid)?, node.as_deref(), follow)?,
        Command::Stop { pid } => {
            let pid = pick(pid)?;
            Client::connect(pid)?.stop()?;
            println!("stopping dataflow {pid}");
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// The daemon to talk to: the given one, or the only one running.
fn pick(pid: Option<u32>) -> Result<u32, String> {
    if let Some(pid) = pid {
        return Ok(pid);
    }
    match runtime::running_daemons()[..] {
        [] => Err("no dataflow is running".into()),
        [pid] => Ok(pid),
        ref pids => {
            let pids: Vec<String> = pids.iter().map(u32::to_string).collect();
            Err(format!("several dataflows are running ({}); pick one with --pid", pids.join(", ")))
        }
    }
}

fn ps() -> Result<(), Box<dyn Error>> {
    println!("{:<8} {:>8} {:>6}  {:<9} DATAFLOW", "PID", "UPTIME", "NODES", "STATE");
    for pid in runtime::running_daemons() {
        // A daemon may exit between listing and connecting.
        let Ok(status) = Client::connect(pid).and_then(|mut c| c.status()) else { continue };
        let running = status.nodes.iter().filter(|n| n.state == NodeState::Running).count();
        println!(
            "{:<8} {:>8} {:>6}  {:<9} {}",
            pid,
            fmt::duration(Duration::from_millis(status.uptime_ms)),
            format!("{running}/{}", status.nodes.len()),
            if status.stopping { "stopping" } else { "running" },
            status.dataflow.display()
        );
    }
    Ok(())
}

fn logs(pid: u32, node: Option<&str>, follow: bool) -> Result<(), Box<dyn Error>> {
    let mut client = Client::connect(pid)?;
    let print = |line: &LogLine| {
        if node.is_none_or(|n| n == line.node) {
            println!("{:>9} {:<12} {}", fmt::timestamp(line.t_ms), line.node, line.text);
        }
    };
    let (mut since, mut fetched) = (0, false);
    loop {
        let logs = match client.logs(since) {
            Ok(logs) => logs,
            // The dataflow finished while we were following it.
            Err(_) if fetched => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        logs.lines.iter().for_each(print);
        (since, fetched) = (logs.next, true);
        if !follow {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}
