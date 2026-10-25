//! A node's checks, for `keel doctor <dataflow>`.
//!
//! The doctor asks a node what it needs by starting it with `KEEL_CHECK` set
//! in its environment. The node does its setup as it always does (reads its
//! arguments, opens its devices) and may [`report`] what's worth saying;
//! then [`crate::Node::from_env`] ends it, before it would join a dataflow.
//! A setup that fails is a failed check, with the node's own error message,
//! which should say how to fix it.
//!
//! So a node's setup before `Node::from_env` must be harmless: open, read,
//! ask; don't move anything.

use std::io::Write;

/// Set in a node's environment when `keel doctor` starts it.
pub const ENV: &str = "KEEL_CHECK";

/// What a line of a node's report starts with, on its stdout.
pub const PREFIX: &str = "keel-check";

/// Whether this process was started by `keel doctor`, to check.
pub fn requested() -> bool {
    std::env::var_os(ENV).is_some()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    /// Works, with a cost worth knowing, or not yet: something to start first.
    Warn,
    /// Won't work.
    Fail,
}

impl Level {
    pub fn name(self) -> &'static str {
        match self {
            Level::Ok => "ok",
            Level::Warn => "warn",
            Level::Fail => "fail",
        }
    }
}

/// Reports a check to `keel doctor`: a line on stdout,
/// `keel-check <TAB> level <TAB> what <TAB> detail`.
pub fn report(level: Level, what: &str, detail: &str) {
    let clean = |s: &str| s.replace(['\t', '\n'], " ");
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{PREFIX}\t{}\t{}\t{}", level.name(), clean(what), clean(detail));
    let _ = out.flush();
}
