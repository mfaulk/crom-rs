use subagent_demo::parse_token;

#[test]
// Standard `v<N>` parses as the numeric portion.
fn parses_standard_token() {
    assert_eq!(parse_token("v1"), Some(1));
    assert_eq!(parse_token("v999"), Some(999));
}

#[test]
// Tokens that don't start with `v` are rejected.
fn rejects_missing_v_prefix() {
    assert_eq!(parse_token("123"), None);
    assert_eq!(parse_token("x123"), None);
}

// Note: there is intentionally NO test here for the empty-string edge case.
// The docstring promises `parse_token("")` returns None, but no test
// covers it — the test-analyst subagent should flag the gap.
