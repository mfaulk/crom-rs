//! A tiny crate that intentionally ships with one buggy parsing function.

/// Parse a token of the form "v123" and return the numeric portion.
///
/// Bug: the off-by-one slice drops the first digit so it returns the wrong
/// value. The accompanying test in `tests/auth_tests.rs` documents the
/// expected behavior.
pub fn parse_token(input: &str) -> Option<u32> {
    let rest = input.strip_prefix('v')?;
    // BUG: should be `rest`, not `&rest[1..]`.
    rest[1..].parse::<u32>().ok()
}
