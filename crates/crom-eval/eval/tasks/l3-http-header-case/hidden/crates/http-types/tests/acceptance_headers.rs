use http_types::Headers;

#[test]
fn lookups_ignore_case() {
    let mut h = Headers::new();
    h.append("Content-Type", "text/html");
    h.append("set-cookie", "a=1");
    h.append("Set-Cookie", "b=2");
    assert_eq!(h.get("content-type"), Some("text/html"));
    assert_eq!(h.get("CONTENT-TYPE"), Some("text/html"));
    assert!(h.contains("SET-COOKIE"));
    assert_eq!(h.get_all("Set-cookie"), ["a=1", "b=2"]);
}

#[test]
fn insert_replaces_any_casing_and_keeps_new_casing() {
    let mut h = Headers::new();
    h.append("x-request-id", "1");
    h.append("X-Request-Id", "2");
    h.append("Accept", "*/*");
    h.insert("X-REQUEST-ID", "3");
    assert_eq!(h.len(), 2);
    let names: Vec<&str> = h.iter().map(|(n, _)| n).collect();
    assert!(names.contains(&"X-REQUEST-ID"));
    assert!(names.contains(&"Accept"));
    assert_eq!(h.get("x-request-id"), Some("3"));
}

#[test]
fn remove_ignores_case() {
    let mut h = Headers::new();
    h.append("Vary", "Origin");
    h.append("vary", "Accept-Encoding");
    h.append("Etag", "\"x\"");
    assert_eq!(h.remove("VARY"), 2);
    assert_eq!(h.iter().collect::<Vec<_>>(), [("Etag", "\"x\"")]);
}
