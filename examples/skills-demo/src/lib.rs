//! Tiny crate used by the M7 skills demo.
//!
//! The crate has just enough public API for the `rust-api-review` Skill
//! to find something concrete to talk about, plus a `CHANGELOG.md` for
//! the `release-pr` Skill to summarize.

/// A simple greeter.
pub struct Greeter {
    name: String,
}

impl Greeter {
    /// Construct a new greeter.
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }

    /// Build a greeting message.
    pub fn greet(&self) -> String {
        format!("Hello, {}!", self.name)
    }
}

/// Add two numbers and return the result.
pub fn add(a: i64, b: i64) -> i64 {
    a + b
}
