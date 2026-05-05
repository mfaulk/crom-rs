use checkpoint_demo::parse_tokens;

#[test]
// FAILING by design: parse_tokens currently does not trim whitespace.
// The agent task is to fix the parser AND add a passing test alongside
// this one. After the agent's run, validation (`cargo test`) should pass.
fn tokens_are_trimmed() {
    assert_eq!(parse_tokens(" a , b , c "), vec!["a", "b", "c"]);
}
