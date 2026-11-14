use slugify::slugify;

/// Runs of mixed separators collapse to a single dash.
#[test]
fn collapses_separator_runs() {
    assert_eq!(slugify("a  b"), "a-b");
    assert_eq!(slugify("Rust -- is __ fun!"), "rust-is-fun");
    assert_eq!(slugify("one/two\\three"), "one-two-three");
}

/// Leading/trailing separators are dropped and plain input is unchanged.
#[test]
fn trims_and_preserves() {
    assert_eq!(slugify("  --Hello, World!--  "), "hello-world");
    assert_eq!(slugify("abc123"), "abc123");
    assert_eq!(slugify("!!!"), "");
    assert_eq!(slugify(""), "");
}
