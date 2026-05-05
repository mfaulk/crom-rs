#[test]
// The demo fixture exposes the parser source hint used by the fake issue tools.
fn parser_hint_points_to_parser() {
    assert_eq!(tool_search_demo::parser_hint(), "src/parser.rs");
}
