//! The dataflow file: which nodes to run and how their outputs feed inputs.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Dataflow {
    pub nodes: Vec<NodeConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeConfig {
    pub id: String,
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
        Ok(Graph { routes, upstream })
    }
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
    fn rejects_invalid_dataflows() {
        let duplicate = "nodes: [{ id: a, path: a }, { id: a, path: b }]";
        let unknown_node = "nodes: [{ id: a, path: a, inputs: { x: nope/out } }]";
        let no_output = "nodes: [{ id: a, path: a, inputs: { x: a } }]";
        let unknown_field = "nodes: [{ id: a, path: a, typo: 1 }]";
        for yaml in [duplicate, unknown_node, no_output, unknown_field] {
            assert!(resolve(yaml).is_err(), "accepted: {yaml}");
        }
    }
}
