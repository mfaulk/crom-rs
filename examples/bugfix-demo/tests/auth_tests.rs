use bugfix_demo::parse_token;

#[test]
// `parse_token("v123")` should return the integer that follows the leading "v".
fn parse_token_extracts_number() {
    assert_eq!(parse_token("v123"), Some(123));
}

#[test]
// `parse_token("v0")` should return Some(0).
fn parse_token_handles_zero() {
    assert_eq!(parse_token("v0"), Some(0));
}

#[test]
// Inputs without the leading "v" return None.
fn parse_token_rejects_missing_prefix() {
    assert_eq!(parse_token("123"), None);
}
