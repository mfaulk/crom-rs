//! Heading text -> anchor.

/// Lowercase, replace runs of non-alphanumerics with `-`, trim dashes.
pub fn slug(text: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            dash = false;
        } else if !dash {
            out.push('-');
            dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic() {
        assert_eq!(slug("Getting Started"), "getting-started");
        assert_eq!(slug("Hello, World!"), "hello-world");
        assert_eq!(slug("API v2"), "api-v2");
    }
}
