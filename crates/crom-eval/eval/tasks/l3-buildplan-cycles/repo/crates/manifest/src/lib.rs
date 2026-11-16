//! `target: dep dep ...` manifests. `#` starts a comment.

use std::collections::BTreeMap;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    /// Target name -> its direct dependencies, in declaration order.
    pub targets: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestError {
    Syntax { line: usize, text: String },
    DuplicateTarget { line: usize, name: String },
    UndefinedDependency { target: String, dep: String },
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ManifestError::Syntax { line, text } => write!(f, "line {line}: cannot parse `{text}`"),
            ManifestError::DuplicateTarget { line, name } => {
                write!(f, "line {line}: target `{name}` defined twice")
            }
            ManifestError::UndefinedDependency { target, dep } => {
                write!(f, "`{target}` depends on undefined target `{dep}`")
            }
        }
    }
}

impl std::error::Error for ManifestError {}

fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
}

pub fn parse(src: &str) -> Result<Manifest, ManifestError> {
    let mut targets: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (i, raw) in src.lines().enumerate() {
        let line_no = i + 1;
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let (name, deps) = line.split_once(':').ok_or_else(|| ManifestError::Syntax {
            line: line_no,
            text: line.to_string(),
        })?;
        let name = name.trim();
        if !valid_name(name) {
            return Err(ManifestError::Syntax {
                line: line_no,
                text: line.to_string(),
            });
        }
        let deps: Vec<String> = deps.split_whitespace().map(str::to_string).collect();
        if let Some(bad) = deps.iter().find(|d| !valid_name(d)) {
            return Err(ManifestError::Syntax {
                line: line_no,
                text: bad.clone(),
            });
        }
        if targets.insert(name.to_string(), deps).is_some() {
            return Err(ManifestError::DuplicateTarget {
                line: line_no,
                name: name.to_string(),
            });
        }
    }
    for (target, deps) in &targets {
        for dep in deps {
            if !targets.contains_key(dep) {
                return Err(ManifestError::UndefinedDependency {
                    target: target.clone(),
                    dep: dep.clone(),
                });
            }
        }
    }
    Ok(Manifest { targets })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_targets_and_comments() {
        let m = parse("# demo\napp: core net  # main\nnet: core\ncore:\n").unwrap();
        assert_eq!(m.targets["app"], ["core", "net"]);
        assert!(m.targets["core"].is_empty());
    }

    #[test]
    fn reports_errors() {
        assert_eq!(
            parse("a: b\n"),
            Err(ManifestError::UndefinedDependency {
                target: "a".into(),
                dep: "b".into()
            })
        );
        assert!(matches!(
            parse("a b c"),
            Err(ManifestError::Syntax { line: 1, .. })
        ));
        assert!(matches!(
            parse("a:\na:\n"),
            Err(ManifestError::DuplicateTarget { line: 2, .. })
        ));
    }
}
