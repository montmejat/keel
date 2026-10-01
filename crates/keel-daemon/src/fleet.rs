//! A fleet: several robots running the same dataflow.
//!
//! ```yaml
//! dataflow: pipeline-image.yml      # relative to this file
//! robots:
//!   robot-1: { robot: 127.0.0.1:7411 }   # the dataflow's machines, on this robot
//!   robot-2: { robot: 127.0.0.1:7412 }
//! ```
//!
//! Each robot is the dataflow with its machines at that robot's addresses,
//! deployed under its own name (`<dataflow>-<robot>`), so each has its own
//! history, rollback and branches (see `packaging`), and its own coordinator
//! when it runs. What the fleet adds is doing things to several at once, or
//! to one before the others.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::dataflow::Dataflow;
use crate::packaging::{self, Deployment};
use crate::sha256;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fleet {
    /// The dataflow every robot runs. Its `machines:` names the machines a
    /// robot is made of; the addresses there are placeholders.
    pub dataflow: PathBuf,
    /// Robot -> machine -> address of the daemon there.
    pub robots: BTreeMap<String, BTreeMap<String, String>>,
}

impl Fleet {
    pub fn load(path: &Path) -> io::Result<Self> {
        let path = std::fs::canonicalize(path)?;
        let mut fleet: Fleet = serde_yaml::from_str(&std::fs::read_to_string(&path)?)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{}: {e}", path.display())))?;
        fleet.dataflow = std::fs::canonicalize(path.parent().unwrap().join(&fleet.dataflow))?;
        let template = Dataflow::load(&fleet.dataflow)?;
        for (robot, machines) in &fleet.robots {
            if !machines.keys().eq(template.machines.keys()) {
                return Err(io::Error::other(format!(
                    "`{robot}` must give an address for each machine of {}: {}",
                    fleet.dataflow.display(),
                    template.machines.keys().cloned().collect::<Vec<_>>().join(", ")
                )));
            }
        }
        Ok(fleet)
    }

    /// The robots asked for, or all of them.
    pub fn select(&self, only: &[String]) -> io::Result<Vec<String>> {
        if let Some(unknown) = only.iter().find(|robot| !self.robots.contains_key(*robot)) {
            return Err(io::Error::other(format!("no robot `{unknown}` in the fleet")));
        }
        Ok(self.robots.keys().filter(|robot| only.is_empty() || only.contains(robot)).cloned().collect())
    }

    /// What `robot`'s deployments are recorded as.
    pub fn name(&self, robot: &str) -> String {
        format!("{}-{robot}", self.dataflow.file_stem().unwrap_or_default().to_string_lossy())
    }

    /// Builds the dataflow and ships it to `robot`'s machines.
    pub fn deploy(&self, robot: &str) -> io::Result<Deployment> {
        let mut dataflow = Dataflow::load(&self.dataflow)?;
        dataflow.machines = self.robots[robot].clone();
        packaging::deploy_as(self.name(robot), self.dataflow.clone(), dataflow)
    }
}

/// A short name for the code a deployment runs: the same on every robot
/// running the same binaries, whatever their addresses.
pub fn code(deployment: &Deployment) -> String {
    let binaries: Vec<String> = deployment.binaries.iter().map(|(node, hash)| format!("{node}={hash}")).collect();
    sha256::hash(binaries.join("\n").as_bytes())[..8].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_and_select() {
        let dir = std::env::temp_dir().join(format!("keel-fleet-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dataflow = "machines: { arm: 'a:1', base: 'b:1' }\nnodes:\n  - { id: n, machine: arm, path: n }\n";
        std::fs::write(dir.join("robot.yml"), dataflow).unwrap();
        let write = |robots: &str| {
            std::fs::write(dir.join("fleet.yml"), format!("dataflow: robot.yml\nrobots:\n{robots}")).unwrap();
            Fleet::load(&dir.join("fleet.yml"))
        };

        let fleet = write("  r1: { arm: '10.0.0.1:7400', base: '10.0.0.2:7400' }\n  r2: { arm: 'c:1', base: 'd:1' }\n")
            .unwrap();
        assert_eq!(fleet.name("r1"), "robot-r1");
        assert_eq!(fleet.select(&[]).unwrap(), ["r1", "r2"]);
        assert_eq!(fleet.select(&["r2".into()]).unwrap(), ["r2"]);
        assert!(fleet.select(&["r3".into()]).is_err());
        assert!(write("  r1: { arm: '10.0.0.1:7400' }\n").is_err(), "a machine without an address");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
