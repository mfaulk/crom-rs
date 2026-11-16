//! Semantic versions (https://semver.org) and version requirements.

mod req;
mod version;

pub use req::{Comparator, Op, VersionReq};
pub use version::{Identifier, Version};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError(pub String);

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid version syntax: {}", self.0)
    }
}

impl std::error::Error for ParseError {}
