//! A directed dependency graph. An edge `a -> b` means "a depends on b", so
//! `b` must be built before `a`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphError {
    UnknownNode(String),
    /// The nodes of a dependency cycle.
    Cycle(Vec<String>),
}

impl fmt::Display for GraphError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GraphError::UnknownNode(n) => write!(f, "unknown node `{n}`"),
            GraphError::Cycle(nodes) => write!(f, "dependency cycle among {}", nodes.join(", ")),
        }
    }
}

impl std::error::Error for GraphError {}

#[derive(Debug, Clone, Default)]
pub struct Graph {
    deps: BTreeMap<String, BTreeSet<String>>,
}

impl Graph {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_node(&mut self, name: &str) {
        self.deps.entry(name.to_string()).or_default();
    }

    /// Record that `from` depends on `to`. Both nodes must exist.
    pub fn add_dep(&mut self, from: &str, to: &str) -> Result<(), GraphError> {
        if !self.deps.contains_key(to) {
            return Err(GraphError::UnknownNode(to.to_string()));
        }
        self.deps
            .get_mut(from)
            .ok_or_else(|| GraphError::UnknownNode(from.to_string()))?
            .insert(to.to_string());
        Ok(())
    }

    pub fn contains(&self, name: &str) -> bool {
        self.deps.contains_key(name)
    }

    pub fn deps_of(&self, name: &str) -> impl Iterator<Item = &str> {
        self.deps
            .get(name)
            .into_iter()
            .flat_map(|s| s.iter().map(String::as_str))
    }

    pub fn nodes(&self) -> impl Iterator<Item = &str> {
        self.deps.keys().map(String::as_str)
    }

    /// The sub-graph of `root` and everything it transitively depends on.
    pub fn reachable_from(&self, root: &str) -> Result<Graph, GraphError> {
        if !self.contains(root) {
            return Err(GraphError::UnknownNode(root.to_string()));
        }
        let mut out = Graph::new();
        let mut stack = vec![root.to_string()];
        while let Some(n) = stack.pop() {
            if out.contains(&n) {
                continue;
            }
            out.deps.insert(n.clone(), self.deps[&n].clone());
            stack.extend(self.deps[&n].iter().cloned());
        }
        Ok(out)
    }

    /// Dependencies-first order. Among nodes that are ready at the same time,
    /// the alphabetically smallest comes first.
    pub fn topo_order(&self) -> Result<Vec<String>, GraphError> {
        let mut remaining: BTreeMap<&str, usize> = self
            .deps
            .iter()
            .map(|(n, d)| (n.as_str(), d.len()))
            .collect();
        let mut dependents: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for (n, ds) in &self.deps {
            for d in ds {
                dependents.entry(d.as_str()).or_default().push(n.as_str());
            }
        }
        let mut ready: BTreeSet<&str> = remaining
            .iter()
            .filter(|(_, &c)| c == 0)
            .map(|(n, _)| *n)
            .collect();
        let mut order = Vec::with_capacity(self.deps.len());
        while let Some(n) = ready.pop_first() {
            order.push(n.to_string());
            for &m in dependents.get(n).map(Vec::as_slice).unwrap_or(&[]) {
                let c = remaining.get_mut(m).expect("node exists");
                *c -= 1;
                if *c == 0 {
                    ready.insert(m);
                }
            }
        }
        Ok(order)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(edges: &[(&str, &str)], nodes: &[&str]) -> Graph {
        let mut g = Graph::new();
        for n in nodes {
            g.add_node(n);
        }
        for (a, b) in edges {
            g.add_dep(a, b).unwrap();
        }
        g
    }

    #[test]
    fn orders_dependencies_first() {
        let g = g(
            &[("app", "net"), ("app", "core"), ("net", "core")],
            &["app", "net", "core"],
        );
        assert_eq!(g.topo_order().unwrap(), ["core", "net", "app"]);
    }

    #[test]
    fn ties_break_alphabetically() {
        let g = g(&[("z", "b"), ("z", "a")], &["z", "b", "a", "c"]);
        assert_eq!(g.topo_order().unwrap(), ["a", "b", "c", "z"]);
    }

    #[test]
    fn unknown_nodes_rejected() {
        let mut g = Graph::new();
        g.add_node("a");
        assert_eq!(
            g.add_dep("a", "b"),
            Err(GraphError::UnknownNode("b".into()))
        );
    }
}
