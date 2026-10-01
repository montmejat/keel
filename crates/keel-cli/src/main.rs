//! `keel`: run dataflows and inspect the running ones.
//!
//! Every command except `run` is a client of a daemon's control API.

mod fmt;
mod recording;
mod top;
mod trace;

use std::error::Error;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use keel_daemon::control::{Client, LogLine, NodeState};
use keel_daemon::packaging::{self, Registry};
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
    /// machines it lists. Deploys it first if it has nodes to build
    Run { dataflow: PathBuf },
    /// Build a dataflow's nodes for their machines and ship them, without
    /// running it
    Deploy { dataflow: PathBuf },
    /// Run a deployment: a dataflow name's current one, or one by id
    Start { deployment: String },
    /// List the deployments made from this machine
    History { name: Option<String> },
    /// Make the deployment before the current one (or the given one) current
    Rollback { name: String, id: Option<String> },
    /// Forget old deployments, and delete binaries no deployment uses
    Gc {
        /// Deployments to keep per dataflow name, besides the current one
        #[arg(long, default_value_t = packaging::DEFAULT_KEEP)]
        keep: usize,
    },
    /// Make a machine reachable over SSH a keel machine: install keel, share
    /// the token, run the daemon as a service
    Provision {
        /// As `ssh` knows it (your ~/.ssh/config applies)
        host: String,
        /// Where the daemon listens. Only connections presenting the token
        /// are accepted
        #[arg(long, default_value = "0.0.0.0:7400")]
        listen: String,
        /// Stop and remove keel from the host instead (keeps its store)
        #[arg(long)]
        remove: bool,
    },
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
    /// What's in a recording
    Recording { file: PathBuf },
    /// Run a dataflow on this machine with its recorded source nodes
    /// replaced by the recording
    Replay {
        recording: PathBuf,
        dataflow: PathBuf,
        /// Playback speed; 0 for as fast as possible
        #[arg(long, default_value_t = 1.0)]
        speed: f64,
    },
    /// Write a recording's messages out as files, with an index
    Export {
        recording: PathBuf,
        dir: PathBuf,
        /// Only this channel (an input name or `node/output`); repeatable
        #[arg(long = "channel")]
        channels: Vec<String>,
        /// Seconds from the start of the recording
        #[arg(long, default_value_t = 0.0)]
        from: f64,
        #[arg(long, default_value_t = f64::INFINITY)]
        to: f64,
    },
    /// The node `keel replay` puts in place of a recorded one
    #[command(hide = true)]
    ReplayNode {
        recording: PathBuf,
        #[arg(long, default_value_t = 1.0)]
        speed: f64,
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
        Command::Deploy { dataflow } => {
            packaging::deploy(&dataflow)?;
        }
        Command::Start { deployment } => {
            let ok = keel_daemon::start(&Registry::open()?.find(&deployment)?)?;
            return Ok(if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE });
        }
        Command::History { name } => history(name.as_deref())?,
        Command::Rollback { name, id } => {
            let d = Registry::open()?.rollback(&name, id.as_deref())?;
            println!("`{name}` is now {} ({}); `keel start {name}` runs it", d.id, fmt::age(d.created));
        }
        Command::Gc { keep } => packaging::gc(keep)?,
        Command::Recording { file } => recording::info(&file)?,
        Command::Replay { recording, dataflow, speed } => {
            let ok = recording::replay(&recording, &dataflow, speed)?;
            return Ok(if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE });
        }
        Command::Export { recording, dir, channels, from, to } => {
            recording::export(&recording, &dir, &channels, from, to)?
        }
        Command::ReplayNode { recording, speed } => recording::replay_node(&recording, speed)?,
        Command::Provision { host, listen, remove: true } => {
            let _ = listen;
            keel_daemon::provision::remove(&host)?
        }
        Command::Provision { host, listen, remove: false } => {
            let workspace = packaging::workspace_root(&std::env::current_dir()?)?;
            keel_daemon::provision::provision(&host, &listen, &workspace)?
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

fn history(name: Option<&str>) -> Result<(), Box<dyn Error>> {
    let registry = Registry::open()?;
    let names = match name {
        Some(name) => vec![name.to_owned()],
        None => registry.names(),
    };
    println!("  {:<22} {:<13} {:>10}  {:>5}  {:<16} RUSTC", "NAME", "ID", "DEPLOYED", "BUILT", "MACHINES");
    for name in names {
        let current = registry.current(&name).ok().map(|d| d.id);
        for d in registry.history(&name)? {
            let machines: Vec<&str> = d.dataflow.machines.keys().map(String::as_str).collect();
            println!(
                "{} {:<22} {:<13} {:>10}  {:>5}  {:<16} {}",
                if current.as_ref() == Some(&d.id) { "*" } else { " " },
                d.name,
                d.id,
                fmt::age(d.created),
                d.binaries.len(),
                if machines.is_empty() { "(this one)".into() } else { machines.join(", ") },
                d.rustc.strip_prefix("rustc ").unwrap_or(&d.rustc).split(' ').next().unwrap_or(""),
            );
        }
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
