use std::fmt;
use std::str::FromStr;

use crate::{ParseError, Version};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Exact,
    Greater,
    GreaterEq,
    Less,
    LessEq,
    Tilde,
    Caret,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comparator {
    pub op: Op,
    pub version: Version,
}

impl Comparator {
    fn matches_release(&self, v: &Version) -> bool {
        let c = &self.version;
        match self.op {
            Op::Exact => v == c,
            Op::Greater => v > c,
            Op::GreaterEq => v >= c,
            Op::Less => v < c,
            Op::LessEq => v <= c,
            Op::Tilde => v >= c && v.major == c.major && v.minor == c.minor,
            Op::Caret => {
                if v < c {
                    false
                } else if c.major > 0 {
                    v.major == c.major
                } else if c.minor > 0 {
                    v.major == 0 && v.minor == c.minor
                } else {
                    v.major == 0 && v.minor == 0 && v.patch == c.patch
                }
            }
        }
    }
}

/// A comma-separated list of comparators that must all match.
/// A bare version (`"1.2.3"`) is shorthand for `^1.2.3`; `"*"` matches any
/// version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionReq {
    pub comparators: Vec<Comparator>,
}

impl VersionReq {
    pub const STAR: VersionReq = VersionReq {
        comparators: Vec::new(),
    };

    pub fn parse(s: &str) -> Result<Self, ParseError> {
        s.parse()
    }

    pub fn matches(&self, v: &Version) -> bool {
        self.comparators.iter().all(|c| c.matches_release(v))
    }
}

impl FromStr for VersionReq {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if s == "*" {
            return Ok(VersionReq::STAR);
        }
        let mut comparators = Vec::new();
        for part in s.split(',') {
            let part = part.trim();
            let (op, rest) = if let Some(r) = part.strip_prefix(">=") {
                (Op::GreaterEq, r)
            } else if let Some(r) = part.strip_prefix("<=") {
                (Op::LessEq, r)
            } else if let Some(r) = part.strip_prefix('>') {
                (Op::Greater, r)
            } else if let Some(r) = part.strip_prefix('<') {
                (Op::Less, r)
            } else if let Some(r) = part.strip_prefix('=') {
                (Op::Exact, r)
            } else if let Some(r) = part.strip_prefix('~') {
                (Op::Tilde, r)
            } else if let Some(r) = part.strip_prefix('^') {
                (Op::Caret, r)
            } else {
                (Op::Caret, part)
            };
            comparators.push(Comparator {
                op,
                version: rest.trim().parse()?,
            });
        }
        Ok(VersionReq { comparators })
    }
}

impl fmt::Display for VersionReq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.comparators.is_empty() {
            return f.write_str("*");
        }
        let parts: Vec<String> = self
            .comparators
            .iter()
            .map(|c| {
                let op = match c.op {
                    Op::Exact => "=",
                    Op::Greater => ">",
                    Op::GreaterEq => ">=",
                    Op::Less => "<",
                    Op::LessEq => "<=",
                    Op::Tilde => "~",
                    Op::Caret => "^",
                };
                format!("{op}{}", c.version)
            })
            .collect();
        f.write_str(&parts.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(req: &str, v: &str) -> bool {
        VersionReq::parse(req).unwrap().matches(&v.parse().unwrap())
    }

    #[test]
    fn caret_semantics() {
        assert!(m("^1.2.3", "1.9.0"));
        assert!(!m("^1.2.3", "2.0.0"));
        assert!(m("^0.2.3", "0.2.9"));
        assert!(!m("^0.2.3", "0.3.0"));
        assert!(m("^0.0.3", "0.0.3"));
        assert!(!m("^0.0.3", "0.0.4"));
    }

    #[test]
    fn tilde_and_ranges() {
        assert!(m("~1.2.3", "1.2.9"));
        assert!(!m("~1.2.3", "1.3.0"));
        assert!(m(">=1.0.0, <2.0.0", "1.5.0"));
        assert!(!m(">=1.0.0, <2.0.0", "2.0.0"));
        assert!(m("*", "7.0.0"));
    }

    #[test]
    fn display_roundtrip() {
        let r = VersionReq::parse(">=1.0.0, <2.0.0").unwrap();
        assert_eq!(r.to_string(), ">=1.0.0, <2.0.0");
    }
}
