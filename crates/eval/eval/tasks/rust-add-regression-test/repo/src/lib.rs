/// Convert text into a slug: lowercase ASCII alphanumerics, other runs become
/// a single dash.
///
/// Bug: consecutive non-alphanumeric characters each emit their own dash, so
/// "a  b" becomes "a--b" instead of "a-b".
pub fn slugify(input: &str) -> String {
    let mut out = String::new();
    for c in input.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_slug() {
        assert_eq!(slugify("Hello World"), "hello-world");
    }
}
