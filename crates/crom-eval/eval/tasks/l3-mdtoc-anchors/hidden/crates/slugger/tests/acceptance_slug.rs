use slugger::slug;

#[test]
fn github_compatible_slugs() {
    assert_eq!(slug("Getting Started"), "getting-started");
    assert_eq!(slug("Hello, World!"), "hello-world");
    assert_eq!(slug("C++ & Rust"), "c--rust");
    assert_eq!(slug("a -- b"), "a----b");
    assert_eq!(slug("Version 2.0"), "version-20");
    assert_eq!(slug("snake_case_name"), "snake_case_name");
    assert_eq!(slug("-leading and trailing-"), "-leading-and-trailing-");
}

#[test]
fn unicode_letters_survive() {
    assert_eq!(slug("Über Straße"), "über-straße");
    assert_eq!(slug("日本語 見出し"), "日本語-見出し");
    assert_eq!(slug("Ünïcödé?"), "ünïcödé");
}
