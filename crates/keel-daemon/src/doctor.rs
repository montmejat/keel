//! `keel doctor`: is this machine fit to run a robot?
//!
//! A list of checks, each read from where Linux says it (`/proc`, `/sys`,
//! the process's limits): nothing is measured, so it takes no time and can
//! run on a machine in use. A daemon answers the same list for its machine
//! (`ToDaemon::Diagnose`), so `keel doctor <dataflow>` covers every machine
//! a dataflow runs on, including those with no shell to look around in.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::store::Store;
use crate::wire;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Ok,
    /// Works, with a cost worth knowing.
    Warn,
    /// Something keel offers won't work here.
    Fail,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Check {
    pub name: String,
    pub level: Level,
    /// What was found, and for anything but `Ok`, what it costs and what to
    /// do about it.
    pub detail: String,
}

fn check(name: &str, level: Level, detail: String) -> Check {
    Check { name: name.to_owned(), level, detail }
}

fn read(path: &str) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_owned())
}

/// This machine's checks, as seen by this process: limits are those the
/// nodes it starts would inherit.
pub fn here() -> Vec<Check> {
    vec![
        kernel(),
        realtime_limit(),
        memory_lock(),
        throttling(),
        governor(),
        isolated(),
        clock(),
        swap(),
        shared_memory(),
        token(),
        store(),
    ]
}

fn kernel() -> Check {
    let release = read("/proc/sys/kernel/osrelease").unwrap_or_default();
    let version = read("/proc/sys/kernel/version").unwrap_or_default();
    match read("/sys/kernel/realtime").as_deref() == Some("1") || version.contains("PREEMPT_RT") {
        true => check("kernel", Level::Ok, format!("{release}, PREEMPT_RT")),
        false => {
            let model =
                ["PREEMPT_DYNAMIC", "PREEMPT"].into_iter().find(|m| version.contains(m)).unwrap_or("no preemption");
            let cost = "a real-time node can be woken milliseconds late under load; a PREEMPT_RT kernel bounds that";
            check("kernel", Level::Warn, format!("{release}, {model}: {cost}"))
        }
    }
}

/// A resource limit's soft value; `None` for unlimited. (The resource's type
/// differs between C libraries.)
fn limit(resource: u32) -> Option<u64> {
    let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: plain syscall writing into `limit`.
    unsafe { libc::getrlimit(resource as _, &mut limit) };
    (limit.rlim_cur != libc::RLIM_INFINITY).then_some(limit.rlim_cur)
}

fn root() -> bool {
    // SAFETY: cannot fail.
    unsafe { libc::geteuid() == 0 }
}

fn realtime_limit() -> Check {
    let fix = "nodes with `rt:` run without real-time priority; `keel provision` sets the limit, or add \
               `<user> - rtprio 95` to /etc/security/limits.d/keel.conf";
    match limit(libc::RLIMIT_RTPRIO as u32) {
        _ if root() => check("real-time priority", Level::Ok, "running as root".into()),
        Some(0) => check("real-time priority", Level::Fail, format!("not allowed (rtprio limit 0): {fix}")),
        Some(max) => check("real-time priority", Level::Ok, format!("allowed up to {max}")),
        None => check("real-time priority", Level::Ok, "allowed, no limit".into()),
    }
}

fn memory_lock() -> Check {
    match limit(libc::RLIMIT_MEMLOCK as u32) {
        None => check("locked memory", Level::Ok, "no limit".into()),
        Some(_) if root() => check("locked memory", Level::Ok, "running as root".into()),
        Some(bytes) => check(
            "locked memory",
            Level::Warn,
            format!(
                "limited to {} MiB: a real-time node using more can still take page faults; raise memlock in \
                 limits.d (`keel provision` does)",
                bytes >> 20
            ),
        ),
    }
}

fn throttling() -> Check {
    let runtime = read("/proc/sys/kernel/sched_rt_runtime_us").and_then(|s| s.parse::<i64>().ok());
    let period = read("/proc/sys/kernel/sched_rt_period_us").and_then(|s| s.parse::<i64>().ok());
    match (runtime, period) {
        (Some(runtime), Some(period)) if runtime >= 0 && runtime < period => {
            let percent = 100 * (period - runtime) / period;
            let detail = format!(
                "real-time nodes are paused {percent}% of the time if they never sleep \
                 (sched_rt_runtime_us = {runtime}): the kernel's guard against a stuck one"
            );
            check("real-time throttling", Level::Ok, detail)
        }
        (Some(_), Some(_)) => check(
            "real-time throttling",
            Level::Warn,
            "off: a real-time node stuck in a loop takes its CPU for good; give such nodes `watchdog_ms`".into(),
        ),
        _ => check("real-time throttling", Level::Warn, "can't read /proc/sys/kernel/sched_rt_runtime_us".into()),
    }
}

fn governor() -> Check {
    let mut governors: Vec<String> = (fs::read_dir("/sys/devices/system/cpu").into_iter().flatten().flatten())
        .filter_map(|cpu| read(&cpu.path().join("cpufreq/scaling_governor").to_string_lossy()))
        .collect();
    governors.sort();
    governors.dedup();
    match &governors[..] {
        [] => check("CPU frequency", Level::Ok, "not scaled".into()),
        [only] if only == "performance" => check("CPU frequency", Level::Ok, "governor: performance".into()),
        _ => check(
            "CPU frequency",
            Level::Warn,
            format!(
                "governor: {}: a core that slowed down takes time to speed up again, which shows as jitter; \
                 `performance` avoids it",
                governors.join(", ")
            ),
        ),
    }
}

fn isolated() -> Check {
    match read("/sys/devices/system/cpu/isolated").filter(|cpus| !cpus.is_empty()) {
        Some(cpus) => {
            check("isolated CPUs", Level::Ok, format!("{cpus}: pin real-time nodes there (`rt: {{ cpus: [..] }}`)"))
        }
        None => check(
            "isolated CPUs",
            Level::Warn,
            "none: pinned nodes share their core with everything else; `isolcpus=` on the kernel command line \
             keeps cores free"
                .into(),
        ),
    }
}

fn clock() -> Check {
    match read("/sys/devices/system/clocksource/clocksource0/current_clocksource") {
        Some(source) if ["tsc", "arch_sys_counter", "kvm-clock"].contains(&source.as_str()) => {
            check("clock", Level::Ok, format!("{source}: reading the time costs nanoseconds"))
        }
        Some(source) => check(
            "clock",
            Level::Warn,
            format!("{source}: reading the time is slow, and every message is timestamped several times"),
        ),
        None => check("clock", Level::Warn, "can't tell the clock source".into()),
    }
}

fn swap() -> Check {
    let devices = read("/proc/swaps").map_or(0, |s| s.lines().count().saturating_sub(1));
    match devices {
        0 => check("swap", Level::Ok, "none".into()),
        _ => check(
            "swap",
            Level::Warn,
            "on: nodes that don't lock their memory (those without `rt:`) can be swapped out and stall".into(),
        ),
    }
}

fn shared_memory() -> Check {
    let path = c"/dev/shm";
    // SAFETY: `stats` is written by the call before it's read.
    let stats = unsafe {
        let mut stats: libc::statvfs = std::mem::zeroed();
        (libc::statvfs(path.as_ptr(), &mut stats) == 0).then_some(stats)
    };
    match stats {
        Some(stats) => {
            let free = stats.f_bavail as u64 * stats.f_frsize as u64 >> 20;
            let level = if free < 256 { Level::Warn } else { Level::Ok };
            check("shared memory", level, format!("/dev/shm, {free} MiB free: where payloads live"))
        }
        None => check(
            "shared memory",
            Level::Warn,
            "no /dev/shm: payloads go to the runtime directory, which may be a disk".into(),
        ),
    }
}

fn token() -> Check {
    match wire::load_token() {
        Some(_) => check(
            "token",
            Level::Ok,
            format!("{}: daemons here refuse connections without it", wire::token_path().display()),
        ),
        None => check(
            "token",
            Level::Warn,
            "none: a daemon here accepts any connection, and whoever connects can run programs; `keel provision` \
             shares one"
                .into(),
        ),
    }
}

fn store() -> Check {
    match Store::open() {
        Ok(store) => {
            let (blobs, bytes) = store.usage();
            let dir = store.dir().display().to_string();
            let memory = !Path::new("/proc/mounts").exists() || on_memory(&dir);
            let kind = if memory { ", in memory: emptied by a reboot" } else { "" };
            check("store", Level::Ok, format!("{dir}, {blobs} binaries, {} MiB{kind}", bytes >> 20))
        }
        Err(e) => check("store", Level::Fail, format!("can't open the store: {e}: nothing can be deployed here")),
    }
}

/// Whether `dir` is on a memory file system: the longest mount point that
/// contains it is a `tmpfs`, `ramfs` or the initial `rootfs`.
fn on_memory(dir: &str) -> bool {
    let mounts = read("/proc/mounts").unwrap_or_default();
    let mount = mounts
        .lines()
        .filter_map(|line| {
            let mut fields = line.split(' ');
            let (point, kind) = (fields.nth(1)?, fields.next()?);
            Path::new(dir).starts_with(point).then_some((point.len(), kind))
        })
        .max();
    mount.is_some_and(|(_, kind)| ["tmpfs", "ramfs", "rootfs"].contains(&kind))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_check_answers() {
        let checks = here();
        assert_eq!(checks.len(), 11);
        assert!(checks.iter().all(|c| !c.name.is_empty() && !c.detail.is_empty()));
        let json = serde_json::to_string(&checks).unwrap();
        assert_eq!(serde_json::from_str::<Vec<Check>>(&json).unwrap().len(), checks.len());
    }
}
