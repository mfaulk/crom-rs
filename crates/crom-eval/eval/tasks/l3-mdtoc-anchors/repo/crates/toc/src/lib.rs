//! Table-of-contents rendering.

use mdscan::Heading;

pub const START: &str = "<!-- toc -->";
pub const END: &str = "<!-- /toc -->";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub level: u8,
    pub text: String,
    pub anchor: String,
}

/// TOC entries for headings with `min_level <= level <= max_level`.
pub fn entries(doc: &str, min_level: u8, max_level: u8) -> Vec<Entry> {
    mdscan::headings(doc)
        .into_iter()
        .filter(|h: &Heading| h.level >= min_level && h.level <= max_level)
        .map(|h| Entry {
            anchor: slugger::slug(&h.text),
            level: h.level,
            text: h.text,
        })
        .collect()
}

/// Render entries as a nested Markdown list, two spaces per level below the
/// shallowest entry.
pub fn render(entries: &[Entry]) -> String {
    let base = entries.iter().map(|e| e.level).min().unwrap_or(1);
    let mut out = String::new();
    for e in entries {
        let indent = "  ".repeat((e.level - base) as usize);
        out.push_str(&format!("{indent}- [{}](#{})\n", e.text, e.anchor));
    }
    out
}

/// Replace the text between the TOC markers with a freshly rendered TOC.
/// Returns `None` when the markers are missing or out of order.
pub fn update(doc: &str, min_level: u8, max_level: u8) -> Option<String> {
    let start = doc.find(START)?;
    let end = doc.find(END)?;
    if end < start {
        return None;
    }
    let toc = render(&entries(doc, min_level, max_level));
    Some(format!("{}{START}\n{toc}{}", &doc[..start], &doc[end..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = "# Project\n\n<!-- toc -->\nstale\n<!-- /toc -->\n\n## Install\n\n### From source\n\n## Usage\n";

    #[test]
    fn renders_nested_list() {
        let e = entries(DOC, 2, 3);
        assert_eq!(
            render(&e),
            "- [Install](#install)\n  - [From source](#from-source)\n- [Usage](#usage)\n"
        );
    }

    #[test]
    fn update_replaces_between_markers() {
        let out = update(DOC, 2, 6).unwrap();
        assert!(!out.contains("stale"));
        assert!(out.contains("<!-- toc -->\n- [Install](#install)\n"));
        assert!(out.ends_with("## Usage\n"));
        assert_eq!(update("no markers", 1, 6), None);
    }
}
