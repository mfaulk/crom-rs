pub fn likely_bug_file(issue_title: &str) -> &'static str {
    if issue_title.to_ascii_lowercase().contains("parser") {
        "src/lib.rs"
    } else {
        "README.md"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // Parser-related issue titles point the agent at the library source file.
    fn parser_issue_points_to_lib_rs() {
        assert_eq!(likely_bug_file("Parser trims too late"), "src/lib.rs");
    }
}
