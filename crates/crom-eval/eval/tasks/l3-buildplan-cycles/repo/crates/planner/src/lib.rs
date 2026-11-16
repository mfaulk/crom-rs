//! Build plans for a goal target.

use std::collections::BTreeMap;
use std::fmt;

use depgraph::{Graph, GraphError};
use manifest::{Manifest, ManifestError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// Every target to build, dependencies first.
    pub order: Vec<String>,
    /// Groups of targets that can be built in parallel; wave `i` only depends
    /// on targets in earlier waves.
    pub waves: Vec<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    Manifest(ManifestError),
    Graph(GraphError),
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlanError::Manifest(e) => write!(f, "manifest error: {e}"),
            PlanError::Graph(e) => write!(f, "graph error: {e}"),
        }
    }
}

impl std::error::Error for PlanError {}

impl From<GraphError> for PlanError {
    fn from(e: GraphError) -> Self {
        PlanError::Graph(e)
    }
}

impl From<ManifestError> for PlanError {
    fn from(e: ManifestError) -> Self {
        PlanError::Manifest(e)
    }
}

pub fn graph_of(manifest: &Manifest) -> Graph {
    let mut g = Graph::new();
    for name in manifest.targets.keys() {
        g.add_node(name);
    }
    for (name, deps) in &manifest.targets {
        for d in deps {
            g.add_dep(name, d).expect("manifest validated dependencies");
        }
    }
    g
}

pub fn plan(manifest: &Manifest, goal: &str) -> Result<Plan, PlanError> {
    let sub = graph_of(manifest).reachable_from(goal)?;
    let order = sub.topo_order()?;
    let mut depth: BTreeMap<&str, usize> = BTreeMap::new();
    for n in &order {
        let d = sub.deps_of(n).map(|dep| depth[dep] + 1).max().unwrap_or(0);
        depth.insert(n.as_str(), d);
    }
    let mut waves: Vec<Vec<String>> = Vec::new();
    for n in &order {
        let d = depth[n.as_str()];
        if waves.len() <= d {
            waves.resize(d + 1, Vec::new());
        }
        waves[d].push(n.clone());
    }
    Ok(Plan { order, waves })
}

/// Parse `src` and plan `goal`.
pub fn plan_source(src: &str, goal: &str) -> Result<Plan, PlanError> {
    plan(&manifest::parse(src)?, goal)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = "\
app: cli server
cli: core args
server: core net
net: core
args:
core:
docs:
";

    #[test]
    fn plans_goal_only() {
        let p = plan_source(SRC, "server").unwrap();
        assert_eq!(p.order, ["core", "net", "server"]);
    }

    #[test]
    fn waves_group_parallel_work() {
        let p = plan_source(SRC, "app").unwrap();
        assert_eq!(
            p.waves,
            vec![
                vec!["args".to_string(), "core".to_string()],
                vec!["cli".to_string(), "net".to_string()],
                vec!["server".to_string()],
                vec!["app".to_string()],
            ]
        );
        assert_eq!(p.order.len(), 6);
    }

    #[test]
    fn unknown_goal() {
        assert_eq!(
            plan_source(SRC, "nope"),
            Err(PlanError::Graph(GraphError::UnknownNode("nope".into())))
        );
    }
}
