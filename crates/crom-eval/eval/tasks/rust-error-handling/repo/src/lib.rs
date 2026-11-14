/// Errors a `key = value` line can produce.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// The line contains no `=` separator.
    MissingSeparator,
    /// The key (text before the first `=`, trimmed) is empty.
    EmptyKey,
    /// The value (text after the first `=`, trimmed) is not a valid `i64`.
    /// Carries that trimmed value text.
    InvalidNumber(String),
}

/// Parse a `key = value` line into its trimmed key and integer value.
///
/// The line is split at the first `=`. Checks apply in order: separator,
/// then key, then value.
///
/// Currently this panics on malformed input instead of reporting a
/// [`ParseError`].
pub fn parse_entry(line: &str) -> (String, i64) {
    let (key, value) = line.split_once('=').unwrap();
    let value: i64 = value.trim().parse().unwrap();
    (key.trim().to_string(), value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_entry() {
        assert_eq!(parse_entry("retries = 3"), ("retries".to_string(), 3));
    }
}
