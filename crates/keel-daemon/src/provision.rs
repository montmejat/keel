//! Provisioning: turn a machine reachable over SSH into a keel machine.
//!
//! `keel provision <host>` needs nothing on the host but `sh`, `sha256sum`
//! and systemd, and plain `ssh` here (it uses your `~/.ssh/config`):
//!
//! 1. asks the host its architecture, user, whether it has passwordless sudo
//!    and systemd, and the hash of the `keel` it has, if any;
//! 2. builds `keel` for it (static, see `packaging`) and copies it to
//!    `~/.local/bin/keel` over SSH, unless it already has that exact binary;
//! 3. copies this machine's token (see `wire::open`), creating it first if
//!    needed, so the daemon only accepts connections from us and its peers;
//! 4. installs `keel daemon` as a service: a system service running as the
//!    user when sudo works, which lets it raise the real-time limits its
//!    nodes need, or a user service otherwise (no real-time priority);
//! 5. checks that the daemon answers, with the token.
//!
//! `keel provision --remove <host>` undoes it, keeping the store.

use std::io::{self, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::packaging;
use crate::sha256;
use crate::wire;

const SYSTEM_UNIT: &str = "/etc/systemd/system/keel.service";

/// What `keel provision` learns about a host first.
struct Host {
    arch: String,
    user: String,
    uid: String,
    home: String,
    sudo: bool,
    systemd: bool,
    /// `/run/user/<uid>` exists: the user lingers, or is logged in.
    runtime_dir: bool,
    /// SHA-256 of its `~/.local/bin/keel`, if any.
    keel: Option<String>,
}

pub fn provision(host: &str, listen: &str, workspace: &Path) -> io::Result<()> {
    say(host, "looking around".into());
    let h = probe(host)?;
    if !h.systemd {
        return Err(io::Error::other(format!("{host} has no systemd: run `keel daemon` there yourself")));
    }
    let target = format!("{}-unknown-linux-musl", h.arch);
    if !workspace.join("crates/keel-cli").is_dir() {
        return Err(io::Error::other("run `keel provision` from keel's source tree: it builds keel for the host"));
    }

    say(host, format!("building keel for {target}"));
    let binary = packaging::build(workspace, &target, &["keel"])?.join("keel");
    let hash = sha256::hash_reader(std::fs::File::open(&binary)?)?;
    if h.keel.as_deref() == Some(hash.as_str()) {
        say(host, format!("already has this keel ({})", &hash[..12]));
    } else {
        let size = std::fs::metadata(&binary)?.len();
        say(host, format!("installing keel ({}, {} KiB) in ~/.local/bin", &hash[..12], size >> 10));
        let install = "mkdir -p ~/.local/bin && cat > ~/.local/bin/.keel.new && chmod 755 ~/.local/bin/.keel.new \
                       && mv -f ~/.local/bin/.keel.new ~/.local/bin/keel";
        ssh_with_input(host, install, &std::fs::read(&binary)?)?;
    }

    let token = wire::ensure_token()?;
    ssh_with_input(
        host,
        "mkdir -p ~/.config/keel && umask 077 && cat > ~/.config/keel/token",
        format!("{token}\n").as_bytes(),
    )?;
    say(host, "shared the token: the daemon will only accept connections that present it".into());

    let exec = format!("{}/.local/bin/keel daemon --listen {listen}", h.home);
    if h.sudo {
        let runtime = match h.runtime_dir {
            // So that `keel ps` and `keel top`, run there, find it.
            true => format!("Environment=XDG_RUNTIME_DIR=/run/user/{}\n", h.uid),
            false => String::new(),
        };
        let unit = format!(
            "[Unit]\nDescription=keel daemon\nWants=network-online.target\nAfter=network-online.target\n\n\
             [Service]\nUser={}\nExecStart={exec}\n{runtime}Restart=on-failure\n\
             # Real-time nodes: SCHED_FIFO and locked memory.\nLimitRTPRIO=95\nLimitMEMLOCK=infinity\n\n\
             [Install]\nWantedBy=multi-user.target\n",
            h.user
        );
        let script = format!(
            "sudo tee {SYSTEM_UNIT} > /dev/null && sudo systemctl daemon-reload \
             && sudo systemctl enable --quiet keel.service && sudo systemctl restart keel.service"
        );
        ssh_with_input(host, &script, unit.as_bytes())?;
        say(host, format!("installed {SYSTEM_UNIT} (runs as {}, real-time allowed) and started it", h.user));
    } else {
        let unit = format!(
            "[Unit]\nDescription=keel daemon\n\n[Service]\nExecStart={exec}\nRestart=on-failure\n\n\
             [Install]\nWantedBy=default.target\n"
        );
        let script = "mkdir -p ~/.config/systemd/user && cat > ~/.config/systemd/user/keel.service \
                      && systemctl --user daemon-reload && systemctl --user enable --quiet keel.service \
                      && systemctl --user restart keel.service";
        ssh_with_input(host, script, unit.as_bytes())?;
        if ssh(host, "loginctl enable-linger").is_err() {
            say(host, "couldn't enable lingering: the daemon stops when you log out".into());
        }
        say(host, "installed a user service (no sudo: real-time priority won't be available) and started it".into());
    }

    let address = format!("{}:{}", ssh_hostname(host)?, listen.rsplit(':').next().unwrap_or("7400"));
    let deadline = Instant::now() + Duration::from_secs(10);
    let (target, blobs, _) = loop {
        match packaging::hello(&address) {
            Ok(hello) => break hello,
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(300)),
            Err(e) => return Err(io::Error::other(format!("the daemon doesn't answer at {address}: {e}"))),
        }
    };
    say(host, format!("ready: {target}, {blobs} binaries in its store"));
    println!("\nIn a dataflow:\n\nmachines:\n  {}: {address}", host.split('.').next().unwrap_or(host));
    Ok(())
}

/// Stops and removes the service, the binary and the token. Keeps the store.
pub fn remove(host: &str) -> io::Result<()> {
    let script = format!(
        "if [ -f {SYSTEM_UNIT} ]; then sudo systemctl disable --now --quiet keel.service; \
           sudo rm -f {SYSTEM_UNIT}; sudo systemctl daemon-reload; fi; \
         if [ -f ~/.config/systemd/user/keel.service ]; then systemctl --user disable --now --quiet keel.service; \
           rm -f ~/.config/systemd/user/keel.service; systemctl --user daemon-reload; fi; \
         rm -f ~/.local/bin/keel ~/.config/keel/token"
    );
    ssh(host, &script)?;
    say(host, "removed keel's service, binary and token (its store, ~/.local/share/keel, stays)".into());
    Ok(())
}

fn probe(host: &str) -> io::Result<Host> {
    let script = "uname -m; id -un; id -u; echo \"$HOME\"; \
                  sudo -n true 2>/dev/null && echo sudo || echo nosudo; \
                  command -v systemctl > /dev/null && echo systemd || echo nosystemd; \
                  test -d /run/user/$(id -u) && echo runtime || echo noruntime; \
                  sha256sum ~/.local/bin/keel 2>/dev/null | cut -c1-64";
    let out = ssh(host, script)?;
    let lines: Vec<&str> = out.lines().collect();
    if lines.len() < 7 {
        return Err(io::Error::other(format!("unexpected answer from {host}: {out:?}")));
    }
    Ok(Host {
        arch: lines[0].to_owned(),
        user: lines[1].to_owned(),
        uid: lines[2].to_owned(),
        home: lines[3].to_owned(),
        sudo: lines[4] == "sudo",
        systemd: lines[5] == "systemd",
        runtime_dir: lines[6] == "runtime",
        keel: lines.get(7).map(|s| s.to_string()).filter(|s| s.len() == 64),
    })
}

/// What `ssh` resolves `host` to, from `~/.ssh/config`.
fn ssh_hostname(host: &str) -> io::Result<String> {
    let out = Command::new("ssh").args(["-G", host]).stderr(Stdio::null()).output()?;
    let config = String::from_utf8_lossy(&out.stdout);
    let hostname = config.lines().find_map(|l| l.strip_prefix("hostname "));
    Ok(hostname.unwrap_or(host).to_owned())
}

fn ssh(host: &str, script: &str) -> io::Result<String> {
    ssh_with_input(host, script, &[])
}

/// Runs `script` on `host` with `input` on its stdin; its stdout on success.
fn ssh_with_input(host: &str, script: &str, input: &[u8]) -> io::Result<String> {
    let mut child = Command::new("ssh")
        .args(["-o", "BatchMode=yes", host, script])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child.stdin.take().unwrap().write_all(input)?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        return Err(io::Error::other(format!(
            "on {host}: {}",
            String::from_utf8_lossy(&out.stderr).trim().lines().last().unwrap_or("failed")
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn say(host: &str, text: String) {
    eprintln!("[provision {host}] {text}");
}
