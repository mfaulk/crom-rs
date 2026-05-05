use mcp_demo::likely_bug_file;

#[test]
// The demo crate exposes a deterministic source-file hint for fake parser issues.
fn likely_bug_file_is_deterministic() {
    assert_eq!(likely_bug_file("Parser trims too late"), "src/lib.rs");
}
