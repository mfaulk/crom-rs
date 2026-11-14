//! Greedy newest-compatible resolution.
//!
//! For every requested package the resolver selects the newest non-yanked
//! release that satisfies all requirements seen so far, then recurses into
//! that release's dependencies.

use std::collections::{BTreeMap, VecDeque};
use std::fmt;

use registry::Registry;
use versions::{Version, VersionReq};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    UnknownPackage(String),
    NoMatch { name: String, reqs: Vec<String> },
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResolveError::UnknownPackage(n) => write!(f, "unknown package `{n}`"),
            ResolveError::NoMatch { name, reqs } => {
                write!(f, "no version of `{name}` satisfies {}", reqs.join(" and "))
            }
        }
    }
}

impl std::error::Error for ResolveError {}

/// Newest non-yanked release of `name` matching every requirement.
pub fn select(
    registry: &Registry,
    name: &str,
    reqs: &[VersionReq],
) -> Result<Version, ResolveError> {
    if !registry.contains(name) {
        return Err(ResolveError::UnknownPackage(name.to_string()));
    }
    registry
        .releases(name)
        .iter()
        .rev()
        .filter(|r| !r.yanked)
        .map(|r| &r.version)
        .find(|v| reqs.iter().all(|req| req.matches(v)))
        .cloned()
        .ok_or_else(|| ResolveError::NoMatch {
            name: name.to_string(),
            reqs: reqs.iter().map(|r| r.to_string()).collect(),
        })
}

/// Resolve `roots` and their transitive dependencies to concrete versions.
pub fn resolve(
    registry: &Registry,
    roots: &[(&str, &str)],
) -> Result<BTreeMap<String, Version>, ResolveError> {
    let mut reqs: BTreeMap<String, Vec<VersionReq>> = BTreeMap::new();
    let mut chosen: BTreeMap<String, Version> = BTreeMap::new();
    let mut queue: VecDeque<String> = VecDeque::new();

    for (name, req) in roots {
        let req: VersionReq = req.parse().map_err(|_| ResolveError::NoMatch {
            name: name.to_string(),
            reqs: vec![req.to_string()],
        })?;
        reqs.entry(name.to_string()).or_default().push(req);
        queue.push_back(name.to_string());
    }

    while let Some(name) = queue.pop_front() {
        let version = select(registry, &name, &reqs[&name])?;
        if chosen.get(&name) == Some(&version) {
            continue;
        }
        chosen.insert(name.clone(), version.clone());
        let release = registry
            .releases(&name)
            .iter()
            .find(|r| r.version == version)
            .expect("selected release exists");
        for dep in &release.deps {
            reqs.entry(dep.name.clone())
                .or_default()
                .push(dep.req.clone());
            queue.push_back(dep.name.clone());
        }
    }
    Ok(chosen)
}
