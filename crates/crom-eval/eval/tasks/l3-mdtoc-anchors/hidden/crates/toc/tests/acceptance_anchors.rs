use toc::{entries, render};

#[test]
fn duplicates_get_numeric_suffixes() {
    let doc = "## Example\n## Example\n## Example 1\n## Example\n";
    let anchors: Vec<String> = entries(doc, 1, 6).into_iter().map(|e| e.anchor).collect();
    assert_eq!(
        anchors,
        ["example", "example-1", "example-1-1", "example-2"]
    );
}

#[test]
fn filtered_headings_still_reserve_anchors() {
    let doc = "# Notes\n## Notes\n### C++ & Rust\n";
    let e = entries(doc, 2, 3);
    assert_eq!(
        render(&e),
        "- [Notes](#notes-1)\n  - [C++ & Rust](#c--rust)\n"
    );
}

#[test]
fn code_fence_headings_not_listed() {
    let doc = "## Setup\n```sh\n# install deps\n```\n## Setup\n";
    let anchors: Vec<String> = entries(doc, 1, 6).into_iter().map(|e| e.anchor).collect();
    assert_eq!(anchors, ["setup", "setup-1"]);
}
