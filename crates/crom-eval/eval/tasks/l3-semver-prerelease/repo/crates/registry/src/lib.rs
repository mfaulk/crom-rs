//! An in-memory package index.

use std::collections::BTreeMap;

use versions::{Version, VersionReq};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dependency {
    pub name: String,
    pub req: VersionReq,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: Version,
    pub deps: Vec<Dependency>,
    pub yanked: bool,
}

#[derive(Debug, Default)]
pub struct Registry {
    packages: BTreeMap<String, Vec<Release>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Publish a release. `deps` is a list of `(name, requirement)` strings.
    pub fn publish(&mut self, name: &str, version: &str, deps: &[(&str, &str)]) {
        let release = Release {
            version: version.parse().expect("valid version"),
            deps: deps
                .iter()
                .map(|(n, r)| Dependency {
                    name: n.to_string(),
                    req: r.parse().expect("valid requirement"),
                })
                .collect(),
            yanked: false,
        };
        let entry = self.packages.entry(name.to_string()).or_default();
        entry.retain(|r| r.version != release.version);
        entry.push(release);
        entry.sort_by(|a, b| a.version.cmp(&b.version));
    }

    pub fn yank(&mut self, name: &str, version: &str) -> bool {
        let v: Version = match version.parse() {
            Ok(v) => v,
            Err(_) => return false,
        };
        match self.packages.get_mut(name) {
            Some(rels) => match rels.iter_mut().find(|r| r.version == v) {
                Some(r) => {
                    r.yanked = true;
                    true
                }
                None => false,
            },
            None => false,
        }
    }

    /// All releases of `name`, oldest first.
    pub fn releases(&self, name: &str) -> &[Release] {
        self.packages.get(name).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn contains(&self, name: &str) -> bool {
        self.packages.contains_key(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publish_keeps_releases_sorted() {
        let mut r = Registry::new();
        r.publish("a", "1.2.0", &[]);
        r.publish("a", "1.0.0", &[]);
        r.publish("a", "1.10.0", &[]);
        let vs: Vec<String> = r
            .releases("a")
            .iter()
            .map(|r| r.version.to_string())
            .collect();
        assert_eq!(vs, ["1.0.0", "1.2.0", "1.10.0"]);
    }

    #[test]
    fn yank_marks_release() {
        let mut r = Registry::new();
        r.publish("a", "1.0.0", &[]);
        assert!(r.yank("a", "1.0.0"));
        assert!(!r.yank("a", "2.0.0"));
        assert!(r.releases("a")[0].yanked);
    }
}
