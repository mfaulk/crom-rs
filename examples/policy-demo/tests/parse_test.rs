use policy_demo::{parse_config, parse_kv};

#[test]
fn parses_simple_pair() {
    assert_eq!(parse_kv("FOO=bar"), Some(("FOO".into(), "bar".into())));
}

#[test]
fn ignores_blank_and_comments() {
    assert_eq!(parse_kv(""), None);
    assert_eq!(parse_kv("   "), None);
    assert_eq!(parse_kv("# a comment"), None);
}

#[test]
// A failing baseline: the parser does not currently strip surrounding
// quotes. The agent's job in the demo is to fix this.
fn handles_quoted_values() {
    let v = parse_config("KEY=\"hello world\"\n");
    assert_eq!(v, vec![("KEY".to_string(), "hello world".to_string())]);
}
