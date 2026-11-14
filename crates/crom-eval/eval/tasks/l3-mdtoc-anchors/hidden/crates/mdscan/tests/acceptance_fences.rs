use mdscan::headings;

#[test]
fn headings_inside_fences_are_ignored() {
    let doc = "# Real\n```bash\n# not a heading\n```\n## Also real\n~~~~\n```\n# still code\n~~~~\n### Last\n";
    let got: Vec<(u8, String, usize)> = headings(doc)
        .into_iter()
        .map(|h| (h.level, h.text, h.line))
        .collect();
    assert_eq!(
        got,
        vec![
            (1, "Real".to_string(), 1),
            (2, "Also real".to_string(), 5),
            (3, "Last".to_string(), 10),
        ]
    );
}

#[test]
fn indented_fence_and_short_closer() {
    let doc = "   ````\n# code\n```\n# still code\n````\n# out\n";
    let got: Vec<String> = headings(doc).into_iter().map(|h| h.text).collect();
    assert_eq!(got, ["out"]);
}
