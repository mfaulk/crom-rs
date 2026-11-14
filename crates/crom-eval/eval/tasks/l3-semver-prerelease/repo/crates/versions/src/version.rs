use std::fmt;
use std::str::FromStr;

use crate::ParseError;

/// One dot-separated pre-release identifier.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Identifier {
    Numeric(u64),
    Alpha(String),
}

impl fmt::Display for Identifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Identifier::Numeric(n) => write!(f, "{n}"),
            Identifier::Alpha(s) => f.write_str(s),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    /// Pre-release identifiers; empty for a normal release.
    pub pre: Vec<Identifier>,
}

impl Version {
    pub fn new(major: u64, minor: u64, patch: u64) -> Self {
        Version {
            major,
            minor,
            patch,
            pre: Vec::new(),
        }
    }

    pub fn is_prerelease(&self) -> bool {
        !self.pre.is_empty()
    }

    /// The same version with pre-release identifiers stripped.
    pub fn release(&self) -> Version {
        Version::new(self.major, self.minor, self.patch)
    }

    pub fn parse(s: &str) -> Result<Self, ParseError> {
        s.parse()
    }
}

fn parse_num(part: &str, whole: &str) -> Result<u64, ParseError> {
    if part.is_empty() || (part.len() > 1 && part.starts_with('0')) {
        return Err(ParseError(whole.to_string()));
    }
    part.parse().map_err(|_| ParseError(whole.to_string()))
}

impl FromStr for Version {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        // Build metadata is ignored for precedence.
        let s_no_build = s.split('+').next().unwrap_or("");
        let (core, pre) = match s_no_build.split_once('-') {
            Some((c, p)) => (c, Some(p)),
            None => (s_no_build, None),
        };
        let nums: Vec<&str> = core.split('.').collect();
        if nums.len() != 3 {
            return Err(ParseError(s.to_string()));
        }
        let mut v = Version::new(
            parse_num(nums[0], s)?,
            parse_num(nums[1], s)?,
            parse_num(nums[2], s)?,
        );
        if let Some(pre) = pre {
            for id in pre.split('.') {
                if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
                    return Err(ParseError(s.to_string()));
                }
                if id.chars().all(|c| c.is_ascii_digit()) {
                    v.pre.push(Identifier::Numeric(parse_num(id, s)?));
                } else {
                    v.pre.push(Identifier::Alpha(id.to_string()));
                }
            }
        }
        Ok(v)
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if !self.pre.is_empty() {
            let parts: Vec<String> = self.pre.iter().map(|p| p.to_string()).collect();
            write!(f, "-{}", parts.join("."))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        s.parse().unwrap()
    }

    #[test]
    fn parses_release_and_prerelease() {
        assert_eq!(v("1.2.3"), Version::new(1, 2, 3));
        let p = v("1.0.0-alpha.1");
        assert_eq!(
            p.pre,
            vec![Identifier::Alpha("alpha".into()), Identifier::Numeric(1)]
        );
        assert_eq!(p.to_string(), "1.0.0-alpha.1");
        assert_eq!(v("1.0.0+build.5"), Version::new(1, 0, 0));
    }

    #[test]
    fn rejects_malformed() {
        for bad in ["1.2", "1.2.3.4", "01.2.3", "1.2.x", "1.2.3-", "1.2.3-a..b"] {
            assert!(bad.parse::<Version>().is_err(), "{bad}");
        }
    }

    #[test]
    fn orders_releases_numerically() {
        assert!(v("1.10.0") > v("1.9.9"));
        assert!(v("2.0.0") > v("1.99.99"));
        assert!(v("0.0.2") > v("0.0.1"));
    }

    #[test]
    fn orders_prerelease_identifiers() {
        assert!(v("1.0.0-alpha") < v("1.0.0-alpha.1"));
        assert!(v("1.0.0-alpha.1") < v("1.0.0-alpha.beta"));
        assert!(v("1.0.0-beta.2") < v("1.0.0-beta.11"));
        assert!(v("1.0.0-beta.11") < v("1.0.0-rc.1"));
    }
}
