//! ATX heading extraction.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Heading {
    pub level: u8,
    pub text: String,
    /// 1-based source line.
    pub line: usize,
}

/// Parse one line as an ATX heading.
pub fn parse_heading(line: &str) -> Option<(u8, String)> {
    let trimmed = line.trim_start_matches(' ');
    if line.len() - trimmed.len() > 3 {
        return None; // indented code
    }
    let hashes = trimmed.chars().take_while(|&c| c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = &trimmed[hashes..];
    if !rest.is_empty() && !rest.starts_with(' ') && !rest.starts_with('\t') {
        return None;
    }
    let mut text = rest.trim();
    // Optional closing sequence: ` ###`.
    let without = text.trim_end_matches('#');
    if without.len() != text.len() && (without.is_empty() || without.ends_with(' ')) {
        text = without.trim_end();
    }
    Some((hashes as u8, text.to_string()))
}

/// All headings in a document, in order.
pub fn headings(doc: &str) -> Vec<Heading> {
    doc.lines()
        .enumerate()
        .filter_map(|(i, l)| {
            parse_heading(l).map(|(level, text)| Heading {
                level,
                text,
                line: i + 1,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_levels_and_closing_hashes() {
        assert_eq!(parse_heading("# Title"), Some((1, "Title".into())));
        assert_eq!(parse_heading("### Deep ###"), Some((3, "Deep".into())));
        assert_eq!(parse_heading("## C#"), Some((2, "C#".into())));
        assert_eq!(parse_heading("#NoSpace"), None);
        assert_eq!(parse_heading("####### seven"), None);
        assert_eq!(parse_heading("    # indented"), None);
    }

    #[test]
    fn collects_with_lines() {
        let h = headings("# A\ntext\n## B\n");
        assert_eq!(
            h[1],
            Heading {
                level: 2,
                text: "B".into(),
                line: 3
            }
        );
    }
}
