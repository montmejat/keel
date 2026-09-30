//! `keel`: run dataflows and inspect the running ones.
//!
//! Every command except `run` is a client of a daemon's control API.

mod fmt;
mod top;
mod trace;

use std::error::Error;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use keel_daemon::control::{Client, LogLine, NodeState};
use keel_daemon::{runtime, wire};

#[derive(Parser)]
#[command(name = "keel", version, about = "A minimal robotics-style middleware")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run a dataflow in the foreground: on this machine, or across the
    /// machines it lists
    Run { dataflow: PathBuf },
    /// Run this machine's daemon, which multi-machine dataflows run on
    Daemon {
        /// Address to listen on. Anyone who can reach it can run programs on
        /// this machine: only listen on networks you trust.
        #[arg(long, default_value = wire::DEFAULT_LISTEN)]
        listen: String,
    },
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
    /// Latency of every input, and the latest sampled traces
    Trace {
        /// How many traces to show
        #[arg(short = 'n', long, default_value_t = 3)]
        traces: usize,
        /// Also write the sampled traces as Chrome trace JSON (ui.perfetto.dev)
        #[arg(long)]
        export: Option<PathBuf>,
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
        Command::Daemon { listen } => keel_daemon::serve(&listen)?,
        Command::Ps => ps()?,
        Command::Top { pid } => top::run(pick(pid)?)?,
        Command::Logs { node, follow, pid } => logs(pick(pid)?, node.as_deref(), follow)?,
        Command::Stop { pid } => {
            let pid = pick(pid)?;
            Client::connect(pid)?.stop()?;
            println!("stopping dataflow {pid}");
        }
        Command::Trace { traces, export, pid } => trace::run(pick(pid)?, traces, export.as_deref())?,
    }
    Ok(ExitCode::SUCCESS)
}

/// The daemon to talk to: the given one, the only one running, or the only
/// coordinator, which sees every machine.
fn pick(pid: Option<u32>) -> Result<u32, String> {
    if let Some(pid) = pid {
        return Ok(pid);
    }
    match runtime::running_daemons()[..] {
        [] => Err("no dataflow or daemon is running".into()),
        [pid] => Ok(pid),
        ref pids => {
            let coordinators: Vec<u32> = (pids.iter().copied())
                .filter(|&pid| Client::connect(pid).and_then(|mut c| c.status()).is_ok_and(|s| s.coordinator))
                .collect();
            if let [pid] = coordinators[..] {
                return Ok(pid);
            }
            let pids: Vec<String> = pids.iter().map(u32::to_string).collect();
            Err(format!("several dataflows are running ({}); pick one with --pid", pids.join(", ")))
        }
    }
}

fn ps() -> Result<(), Box<dyn Error>> {
    println!("{:<8} {:>8} {:>6}  {:<9} {:<10} DATAFLOW", "PID", "UPTIME", "NODES", "STATE", "MACHINE");
    for pid in runtime::running_daemons() {
        // A daemon may exit between listing and connecting.
        let Ok(status) = Client::connect(pid).and_then(|mut c| c.status()) else { continue };
        let running = status.nodes.iter().filter(|n| n.state == NodeState::Running).count();
        let state = match &status.dataflow {
            None => "idle",
            Some(_) if status.stopping => "stopping",
            Some(_) => "running",
        };
        println!(
            "{:<8} {:>8} {:>6}  {:<9} {:<10} {}",
            pid,
            fmt::duration(Duration::from_millis(status.uptime_ms)),
            format!("{running}/{}", status.nodes.len()),
            state,
            status.machine.as_deref().unwrap_or(if status.coordinator { "(all)" } else { "-" }),
            status.dataflow.as_ref().map_or("-".into(), |d| d.display().to_string())
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
