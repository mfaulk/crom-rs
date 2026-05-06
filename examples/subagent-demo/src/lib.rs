//! Tiny Rust crate used to exercise the M6 subagent demo end-to-end. The
//! crate intentionally contains a few seeded issues so each subagent has
//! something concrete to find:
//!
//! - **Security**: `compare_secret` is a non-constant-time compare and the
//!   token parser silently accepts arbitrary lengths.
//! - **Tests**: there is no test for the empty-string edge case in
//!   `parse_token`, even though the docstring promises behavior for it.
//! - **API design**: `ApiError` is a public struct with a `message: String`
//!   field but no public constructor and no `Display` impl beyond the
//!   `fmt` plumbing — callers cannot inspect or build it.

/// Errors returned by this crate's small public API.
pub struct ApiError {
    pub message: String,
}

impl std::fmt::Debug for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ApiError({})", self.message)
    }
}

/// Parse a `v<N>` token and return `N`. Returns `None` for malformed input.
///
/// Behavior on edge cases (used by tests):
/// - `parse_token("")` returns `None`.
/// - `parse_token("v")` returns `None`.
/// - `parse_token("v123")` returns `Some(123)`.
pub fn parse_token(s: &str) -> Option<u32> {
    if !s.starts_with('v') {
        return None;
    }
    let rest = &s[1..];
    if rest.is_empty() {
        return None;
    }
    rest.parse::<u32>().ok()
}

/// Compare two secrets for equality. NOTE: this is INTENTIONALLY a
/// short-circuiting compare so the security reviewer subagent has something
/// to flag in the demo.
pub fn compare_secret(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    for (x, y) in a.bytes().zip(b.bytes()) {
        if x != y {
            return false;
        }
    }
    true
}
