//! Packaging and deployment: build nodes for the machines they run on, and
//! ship them by hash.
//!
//! A node with `build: <binary>` is built by `keel` from the dataflow's cargo
//! workspace, as a static (musl) binary for its machine's architecture, with
//! paths remapped so that the same sources and compiler give the same bytes.
//! Its SHA-256 names it: a daemon that already has a binary isn't sent it
//! again, and the one it runs is exactly the one that was built.
//!
//! A [`Deployment`] is the dataflow plus the hash of every built node; its id
//! is derived from those, so deploying unchanged code changes nothing.
//! Deployments made from this machine are recorded, with the current one per
//! dataflow name, so they can be listed, rolled back, and started again.
//! `gc` forgets old ones, and has every daemon delete the binaries no
//! deployment uses any more.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, BufReader};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::dataflow::Dataflow;
use crate::sha256;
use crate::store::{self, Store};
use crate::wire::{self, Event, ToDaemon};

/// Deployments kept per dataflow name by `keel gc`, besides the current one.
pub const DEFAULT_KEEP: usize = 5;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Deployment {
    pub id: String,
    /// The dataflow file's name, without extension: what `start` and
    /// `rollback` refer to.
    pub name: String,
    /// The dataflow file it was deployed from.
    pub source: PathBuf,
    /// Unix seconds.
    pub created: u64,
    /// `rustc --version` of the compiler that built it.
    pub rustc: String,
    pub dataflow: Dataflow,
    /// Where `path:` nodes are found. They're not shipped.
    pub base_dir: PathBuf,
    /// Built node -> SHA-256 of its binary.
    pub binaries: BTreeMap<String, String>,
}

/// The target a machine's binaries are built for.
pub fn host_target() -> String {
    format!("{}-unknown-linux-musl", std::env::consts::ARCH)
}

/// Builds the dataflow's `build:` nodes, sends each machine the binaries it
/// lacks, records the deployment and makes it current.
pub fn deploy(path: &Path) -> io::Result<Deployment> {
    let source = fs::canonicalize(path)?;
    let dataflow = Dataflow::load(&source)?;
    dataflow.resolve()?;
    let base_dir = source.parent().unwrap().to_owned();
    let name = source.file_stem().unwrap().to_string_lossy().into_owned();

    // Which target each built node needs: its machine's, or ours.
    let mut remotes: BTreeMap<String, (Remote, String)> = BTreeMap::new();
    let mut targets: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for node in &dataflow.nodes {
        let Some(bin) = &node.build else { continue };
        let target = match &node.machine {
            None => host_target(),
            Some(machine) => {
                if !remotes.contains_key(machine) {
                    let mut remote = Remote::connect(&dataflow.machines[machine])?;
                    let target = match remote.ask(&ToDaemon::Hello)? {
                        Event::Hello { target, .. } => target,
                        other => return Err(unexpected(machine, other)),
                    };
                    remotes.insert(machine.clone(), (remote, target));
                }
                remotes[machine].1.clone()
            }
        };
        targets.entry(target).or_default().push((node.id.clone(), bin.clone()));
    }

    let workspace = workspace_root(&base_dir)?;
    let mut binaries = BTreeMap::new();
    let mut files: BTreeMap<String, PathBuf> = BTreeMap::new();
    for (target, nodes) in &targets {
        let bins: Vec<&str> = nodes.iter().map(|(_, bin)| bin.as_str()).collect();
        say(format!("building {} for {target}", bins.join(", ")));
        let dir = build(&workspace, target, &bins)?;
        for (node, bin) in nodes {
            let file = dir.join(bin);
            let hash = sha256::hash_reader(fs::File::open(&file)?)?;
            binaries.insert(node.clone(), hash.clone());
            files.insert(hash, file);
        }
    }

    let id = sha256::hash(serde_json::to_string(&(&dataflow, &binaries, &base_dir))?.as_bytes())[..12].to_owned();
    let hashes_on = |machine: Option<&String>| -> Vec<String> {
        let nodes = dataflow.nodes.iter().filter(|n| n.machine.as_ref() == machine);
        let mut hashes: Vec<String> = nodes.filter_map(|n| binaries.get(&n.id).cloned()).collect();
        hashes.sort();
        hashes.dedup();
        hashes
    };
    if dataflow.machines.is_empty() {
        let store = Store::open()?;
        let hashes = hashes_on(None);
        for hash in store.missing(&hashes)? {
            store.put_file(&hash, &files[&hash])?;
        }
        store.pin(&id, &hashes)?;
    } else {
        for (machine, (remote, _)) in &mut remotes {
            let hashes = hashes_on(Some(machine));
            let missing = match remote.ask(&ToDaemon::Missing { hashes: hashes.clone() })? {
                Event::Missing { hashes } => hashes,
                other => return Err(unexpected(machine, other)),
            };
            let bytes: u64 = missing.iter().map(|h| fs::metadata(&files[h]).map_or(0, |m| m.len())).sum();
            say(format!(
                "`{machine}` has {} of {} binaries, sending {}",
                hashes.len() - missing.len(),
                hashes.len(),
                human_bytes(bytes)
            ));
            for hash in &missing {
                upload(&remote.address, hash, &files[hash])?;
            }
            match remote.ask(&ToDaemon::Pin { id: id.clone(), hashes })? {
                Event::Done => {}
                other => return Err(unexpected(machine, other)),
            }
        }
    }

    let registry = Registry::open()?;
    let created = registry.get(&name, &id).map_or_else(|_| now(), |d| d.created);
    let deployment = Deployment { id, name, source, created, rustc: rustc_version(), dataflow, base_dir, binaries };
    registry.record(&deployment)?;
    say(format!("deployed {} as {}", deployment.name, deployment.id));
    Ok(deployment)
}

/// Builds a dataflow's `build:` nodes for this machine, without shipping or
/// recording anything. Returns each built node's binary.
pub fn build_here(dataflow: &Dataflow, base_dir: &Path) -> io::Result<BTreeMap<String, PathBuf>> {
    let nodes: Vec<(&String, &String)> =
        dataflow.nodes.iter().filter_map(|n| Some((&n.id, n.build.as_ref()?))).collect();
    if nodes.is_empty() {
        return Ok(BTreeMap::new());
    }
    let mut bins: Vec<&str> = nodes.iter().map(|(_, bin)| bin.as_str()).collect();
    bins.sort();
    bins.dedup();
    let dir = build(&workspace_root(base_dir)?, &host_target(), &bins)?;
    Ok(nodes.into_iter().map(|(id, bin)| (id.clone(), dir.join(bin))).collect())
}

/// A daemon's target, and how many binaries (and bytes) its store holds.
pub fn hello(address: &str) -> io::Result<(String, u64, u64)> {
    match Remote::connect(address)?.ask(&ToDaemon::Hello)? {
        Event::Hello { target, blobs, bytes } => Ok((target, blobs, bytes)),
        other => Err(unexpected(address, other)),
    }
}

/// Cargo's workspace root for the dataflow's directory.
pub fn workspace_root(dir: &Path) -> io::Result<PathBuf> {
    let out = Command::new("cargo")
        .args(["locate-project", "--workspace", "--message-format", "plain"])
        .current_dir(dir)
        .output()?;
    if !out.status.success() {
        return Err(io::Error::other(format!(
            "`build:` nodes need a cargo workspace around {}: {}",
            dir.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()).parent().unwrap().to_owned())
}

/// Builds `bins` for `target`, reproducibly: release, locked dependencies,
/// static, linked by rust-lld, every local path remapped and debug info
/// stripped. Returns the directory holding the binaries.
pub(crate) fn build(workspace: &Path, target: &str, bins: &[&str]) -> io::Result<PathBuf> {
    let target_dir = workspace.join("target").join("keel");
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let cargo_home = std::env::var_os("CARGO_HOME").map_or(home.join(".cargo"), PathBuf::from);
    let rustup_home = std::env::var_os("RUSTUP_HOME").map_or(home.join(".rustup"), PathBuf::from);
    let remaps = [(workspace, "/keel"), (&target_dir, "/target"), (&cargo_home, "/cargo"), (&rustup_home, "/rustup")];
    let mut rustflags: Vec<String> =
        remaps.iter().map(|(from, to)| format!("--remap-path-prefix={}={to}", from.display())).collect();
    rustflags.push("-Cstrip=debuginfo".into());

    let mut command = Command::new("cargo");
    command
        .args(["build", "--release", "--locked", "--target", target, "--target-dir"])
        .arg(&target_dir)
        .args(bins.iter().flat_map(|b| ["--bin", b]))
        .current_dir(workspace)
        .env(format!("CARGO_TARGET_{}_LINKER", target.to_uppercase().replace('-', "_")), "rust-lld")
        .env("CARGO_ENCODED_RUSTFLAGS", rustflags.join("\x1f"));
    let status = command.status()?;
    if !status.success() {
        return Err(io::Error::other(format!(
            "building for {target} failed (is the target installed? `rustup target add {target}`)"
        )));
    }
    Ok(target_dir.join(target).join("release"))
}

fn rustc_version() -> String {
    Command::new("rustc")
        .arg("--version")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_default()
}

/// Sends one binary over its own connection to a daemon.
fn upload(address: &str, hash: &str, file: &Path) -> io::Result<()> {
    let mut stream = wire::open(address, wire::BLOB)?;
    let len = fs::metadata(file)?.len();
    wire::write_json(&mut stream, &wire::BlobHeader { hash: hash.to_owned(), len })?;
    io::copy(&mut fs::File::open(file)?, &mut stream)?;
    match wire::read_json(&mut BufReader::new(stream))? {
        Some(Event::Done) => Ok(()),
        Some(Event::Error { message }) => Err(io::Error::other(format!("{address} refused {hash}: {message}"))),
        other => Err(io::Error::other(format!("{address}: unexpected answer to an upload: {other:?}"))),
    }
}

/// A connection to a daemon for deployment requests, one answer each.
struct Remote {
    address: String,
    stream: TcpStream,
    reader: BufReader<TcpStream>,
}

impl Remote {
    fn connect(address: &str) -> io::Result<Self> {
        let stream = wire::open(address, wire::COORDINATOR)?;
        Ok(Self { address: address.to_owned(), reader: BufReader::new(stream.try_clone()?), stream })
    }

    fn ask(&mut self, request: &ToDaemon) -> io::Result<Event> {
        wire::write_json(&mut self.stream, request)?;
        match wire::read_json(&mut self.reader)? {
            Some(Event::Error { message }) => Err(io::Error::other(format!("{}: {message}", self.address))),
            Some(event) => Ok(event),
            None => Err(io::Error::other(format!("{} closed the connection", self.address))),
        }
    }
}

fn unexpected(machine: &str, event: Event) -> io::Error {
    io::Error::other(format!("`{machine}` answered {event:?}"))
}

/// Deployments made from this machine, under `deployments/` in the store.
pub struct Registry {
    dir: PathBuf,
}

impl Registry {
    pub fn open() -> io::Result<Self> {
        Ok(Self { dir: Store::open()?.dir().join("deployments") })
    }

    fn record(&self, deployment: &Deployment) -> io::Result<()> {
        let dir = self.dir.join(&deployment.name);
        fs::create_dir_all(&dir)?;
        fs::write(dir.join(format!("{}.json", deployment.id)), serde_json::to_vec_pretty(deployment)?)?;
        self.set_current(&deployment.name, &deployment.id)
    }

    fn set_current(&self, name: &str, id: &str) -> io::Result<()> {
        store::check_id(id)?;
        let dir = self.dir.join(name);
        let temp = dir.join(".current");
        let _ = fs::remove_file(&temp);
        std::os::unix::fs::symlink(format!("{id}.json"), &temp)?;
        // Atomic: `start` never sees a half-switched current.
        fs::rename(temp, dir.join("current"))
    }

    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = (fs::read_dir(&self.dir).into_iter().flatten().flatten())
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// A name's deployments, newest first.
    pub fn history(&self, name: &str) -> io::Result<Vec<Deployment>> {
        let mut all: Vec<Deployment> = (fs::read_dir(self.dir.join(name))?.flatten())
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .filter_map(|e| serde_json::from_slice(&fs::read(e.path()).ok()?).ok())
            .collect();
        all.sort_by(|a, b| b.created.cmp(&a.created).then(b.id.cmp(&a.id)));
        Ok(all)
    }

    pub fn current(&self, name: &str) -> io::Result<Deployment> {
        let link = fs::read_link(self.dir.join(name).join("current"))
            .map_err(|_| io::Error::other(format!("nothing deployed as `{name}`")))?;
        let id = link.file_stem().unwrap().to_string_lossy().into_owned();
        self.get(name, &id)
    }

    fn get(&self, name: &str, id: &str) -> io::Result<Deployment> {
        store::check_id(id)?;
        let json = fs::read(self.dir.join(name).join(format!("{id}.json")))
            .map_err(|_| io::Error::other(format!("no deployment {id} of `{name}`")))?;
        Ok(serde_json::from_slice(&json)?)
    }

    /// A deployment by name (its current one) or by id (or a prefix of it).
    pub fn find(&self, what: &str) -> io::Result<Deployment> {
        if self.dir.join(what).is_dir() {
            return self.current(what);
        }
        let matches: Vec<Deployment> = (self.names().iter())
            .flat_map(|name| self.history(name).unwrap_or_default())
            .filter(|d| d.id.starts_with(what))
            .collect();
        match &matches[..] {
            [one] => Ok(one.clone()),
            [] => Err(io::Error::other(format!("no deployment named or numbered `{what}`"))),
            _ => Err(io::Error::other(format!("`{what}` matches several deployments"))),
        }
    }

    /// Makes `id`, or the deployment before the current one, current.
    pub fn rollback(&self, name: &str, id: Option<&str>) -> io::Result<Deployment> {
        let history = self.history(name)?;
        let target = match id {
            Some(id) => history.iter().find(|d| d.id.starts_with(id)),
            None => {
                let current = self.current(name)?;
                history.iter().skip_while(|d| d.id != current.id).nth(1)
            }
        };
        let target = target.cloned().ok_or_else(|| io::Error::other(format!("nothing to roll `{name}` back to")))?;
        self.set_current(name, &target.id)?;
        Ok(target)
    }
}

/// Forgets all but the newest `keep` deployments of each name (and the
/// current one), then has the machines they ran on delete binaries nothing
/// uses any more. A deployment whose machines can't be reached is kept.
pub fn gc(keep: usize) -> io::Result<()> {
    let registry = Registry::open()?;
    let mut forget: Vec<Deployment> = Vec::new();
    for name in registry.names() {
        let current = registry.current(&name).ok().map(|d| d.id);
        let history = registry.history(&name)?;
        forget.extend(history.into_iter().skip(keep).filter(|d| Some(&d.id) != current.as_ref()));
    }
    // Where each forgotten deployment has binaries: this machine, or
    // daemons by address.
    let mut by_place: BTreeMap<Option<String>, Vec<&Deployment>> = BTreeMap::new();
    for d in &forget {
        if d.dataflow.machines.is_empty() {
            by_place.entry(None).or_default().push(d);
        } else {
            for (machine, address) in &d.dataflow.machines {
                if d.dataflow.nodes.iter().any(|n| n.machine.as_ref() == Some(machine) && n.build.is_some()) {
                    by_place.entry(Some(address.clone())).or_default().push(d);
                }
            }
        }
    }
    let mut unreachable: Vec<String> = Vec::new();
    for (place, deployments) in &by_place {
        let ids: Vec<String> = deployments.iter().map(|d| d.id.clone()).collect();
        let result = match place {
            None => (|| {
                let store = Store::open()?;
                ids.iter().try_for_each(|id| store.unpin(id))?;
                store.gc()
            })(),
            Some(address) => {
                Remote::connect(address).and_then(|mut r| match r.ask(&ToDaemon::Unpin { ids: ids.clone() })? {
                    Event::Collected { blobs, bytes } => Ok((blobs, bytes)),
                    other => Err(unexpected(address, other)),
                })
            }
        };
        let place_name = place.clone().unwrap_or_else(|| "this machine".into());
        match result {
            Ok((blobs, bytes)) => say(format!("{place_name}: removed {blobs} binaries, {}", human_bytes(bytes))),
            Err(e) => {
                say(format!("{place_name}: {e}; keeping its deployments for a later gc"));
                unreachable.extend(ids);
            }
        }
    }
    let mut forgotten = 0;
    for d in forget.iter().filter(|d| !unreachable.contains(&d.id)) {
        fs::remove_file(registry.dir.join(&d.name).join(format!("{}.json", d.id)))?;
        forgotten += 1;
    }
    // Also whatever this machine's store holds that nothing pins.
    let (blobs, bytes) = Store::open()?.gc()?;
    if blobs > 0 {
        say(format!("this machine: removed {blobs} more unused binaries, {}", human_bytes(bytes)));
    }
    say(format!("forgot {forgotten} deployments"));
    Ok(())
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn say(text: String) {
    eprintln!("[deploy] {text}");
}

fn human_bytes(n: u64) -> String {
    match n {
        n if n >= 1 << 20 => format!("{:.1} MiB", n as f64 / (1 << 20) as f64),
        n if n >= 1 << 10 => format!("{:.1} KiB", n as f64 / 1024.0),
        n => format!("{n} B"),
    }
}
