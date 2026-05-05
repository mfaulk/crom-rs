/// Tokenize a comma-separated input. The current implementation does not
/// trim whitespace from individual tokens; the demo task is to refactor
/// it to trim and to add tests.
pub fn parse_tokens(input: &str) -> Vec<String> {
    input
        .split(',')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // Bare tokens with no surrounding whitespace are returned verbatim.
    fn no_whitespace_tokens() {
        assert_eq!(parse_tokens("a,b,c"), vec!["a", "b", "c"]);
    }
}
