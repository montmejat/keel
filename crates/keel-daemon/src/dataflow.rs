//! The dataflow file: which nodes to run, where, and how their outputs feed
//! inputs.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Dataflow {
    /// Machine name -> address of the `keel daemon` running there. Empty
    /// means the whole dataflow runs on this machine, inside `keel run`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub machines: BTreeMap<String, String>,
    pub nodes: Vec<NodeConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeConfig {
    pub id: String,
    /// Where the node runs: a key of `Dataflow::machines`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
    /// Executable, relative to the dataflow file.
    pub path: PathBuf,
    /// `input_id: source_node/output_id`
    #[serde(default)]
    pub inputs: BTreeMap<String, String>,
}

/// `(node, output) -> [(node, input)]`
pub type Routes = HashMap<(String, String), Vec<(String, String)>>;

/// A validated dataflow, ready to run.
#[derive(Debug)]
pub struct Graph {
    pub routes: Routes,
    /// Nodes each node receives inputs from.
    pub upstream: HashMap<String, HashSet<String>>,
    /// Nodes on a cycle. Stopping can't cascade to them from the sources.
    pub cyclic: HashSet<String>,
}

impl Dataflow {
    pub fn load(path: &Path) -> io::Result<Self> {
        Self::parse(&std::fs::read_to_string(path)?).map_err(|e| io::Error::other(format!("{}: {e}", path.display())))
    }

    pub fn parse(yaml: &str) -> io::Result<Self> {
        serde_yaml::from_str(yaml).map_err(io::Error::other)
    }

    /// Validates the dataflow and builds the routing table and upstream sets.
    pub fn resolve(&self) -> io::Result<Graph> {
        let ids: HashSet<&str> = self.nodes.iter().map(|n| n.id.as_str()).collect();
        if ids.len() != self.nodes.len() {
            return Err(io::Error::other("node ids must be unique"));
        }
        // Ids name shared-memory files, so keep them to a safe character set.
        if let Some(bad) = ids
            .iter()
            .find(|id| id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'))
        {
            return Err(io::Error::other(format!("node id `{bad}` may only contain letters, digits, `_` and `-`")));
        }
        for node in &self.nodes {
            match &node.machine {
                None if self.machines.is_empty() => {}
                Some(m) if self.machines.contains_key(m) => {}
                None => {
                    return Err(io::Error::other(format!(
                        "node `{}` needs a `machine:`, since the dataflow lists machines",
                        node.id
                    )))
                }
                Some(m) => return Err(io::Error::other(format!("node `{}` runs on unknown machine `{m}`", node.id))),
            }
        }
        let mut routes = Routes::new();
        let mut upstream = HashMap::new();
        for node in &self.nodes {
            let mut sources = HashSet::new();
            for (input_id, source) in &node.inputs {
                let Some((src_node, src_output)) = source.split_once('/').filter(|(n, _)| ids.contains(n)) else {
                    return Err(io::Error::other(format!(
                        "input `{}/{input_id}`: `{source}` is not of the form <node>/<output> with a known node",
                        node.id
                    )));
                };
                routes
                    .entry((src_node.to_owned(), src_output.to_owned()))
                    .or_default()
                    .push((node.id.clone(), input_id.clone()));
                sources.insert(src_node.to_owned());
            }
            upstream.insert(node.id.clone(), sources);
        }
        let cyclic = ids.iter().filter(|id| reaches(&upstream, id, id)).map(|id| id.to_string()).collect();
        Ok(Graph { routes, upstream, cyclic })
    }
}

/// Whether `to` is upstream of `from`, directly or not.
fn reaches(upstream: &HashMap<String, HashSet<String>>, from: &str, to: &str) -> bool {
    let mut seen = HashSet::new();
    let mut todo: Vec<&str> = upstream[from].iter().map(String::as_str).collect();
    while let Some(id) = todo.pop() {
        if id == to {
            return true;
        }
        if seen.insert(id) {
            todo.extend(upstream[id].iter().map(String::as_str));
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(yaml: &str) -> io::Result<Graph> {
        Dataflow::parse(yaml)?.resolve()
    }

    #[test]
    fn routes_outputs_to_inputs() {
        let graph = resolve(
            "nodes:
              - { id: a, path: a }
              - { id: b, path: b, inputs: { x: a/out } }
              - { id: c, path: c, inputs: { y: a/out, z: b/out } }",
        )
        .unwrap();
        let mut to = graph.routes[&("a".into(), "out".into())].clone();
        to.sort();
        assert_eq!(to, [("b".into(), "x".into()), ("c".into(), "y".into())]);
        assert_eq!(graph.upstream["c"], HashSet::from(["a".into(), "b".into()]));
        assert!(graph.upstream["a"].is_empty());
    }

    #[test]
    fn finds_cycles() {
        let graph = resolve(
            "nodes:
              - { id: a, path: a }
              - { id: b, path: b, inputs: { x: a/out, y: c/out } }
              - { id: c, path: c, inputs: { x: b/out } }
              - { id: d, path: d, inputs: { x: c/out } }
              - { id: e, path: e, inputs: { x: e/out } }",
        )
        .unwrap();
        assert_eq!(graph.cyclic, HashSet::from(["b".into(), "c".into(), "e".into()]));
    }

    #[test]
    fn places_nodes_on_machines() {
        let yaml = "{ machines: { m: 'x:1', n: 'y:2' }, nodes: [{ id: a, path: a, machine: m }, { id: b, path: b, machine: n, inputs: { i: a/o } }] }";
        let dataflow = Dataflow::parse(yaml).unwrap();
        dataflow.resolve().unwrap();
        assert_eq!(dataflow.nodes[1].machine.as_deref(), Some("n"));
    }

    #[test]
    fn rejects_invalid_dataflows() {
        let duplicate = "nodes: [{ id: a, path: a }, { id: a, path: b }]";
        let unknown_node = "nodes: [{ id: a, path: a, inputs: { x: nope/out } }]";
        let no_output = "nodes: [{ id: a, path: a, inputs: { x: a } }]";
        let unknown_field = "nodes: [{ id: a, path: a, typo: 1 }]";
        let bad_id = "nodes: [{ id: ../a, path: a }]";
        let no_machine = "{ machines: { m: 'x:1' }, nodes: [{ id: a, path: a }] }";
        let unknown_machine = "{ machines: { m: 'x:1' }, nodes: [{ id: a, path: a, machine: n }] }";
        let machine_without_machines = "nodes: [{ id: a, path: a, machine: m }]";
        for yaml in [
            duplicate,
            unknown_node,
            no_output,
            unknown_field,
            bad_id,
            no_machine,
            unknown_machine,
            machine_without_machines,
        ] {
            assert!(resolve(yaml).is_err(), "accepted: {yaml}");
        }
    }
}
