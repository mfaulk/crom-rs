//! Diff and change-summary rendering. Uses the `similar` crate to format
//! line-level diffs deterministically. Long diffs are truncated visibly.

use std::path::PathBuf;

use similar::{ChangeTag, TextDiff};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffEntry {
    Modified {
        path: PathBuf,
        before: String,
        after: String,
    },
    Created {
        path: PathBuf,
        after: String,
    },
    Deleted {
        path: PathBuf,
        before: String,
    },
    Binary {
        path: PathBuf,
    },
    Redacted {
        path: PathBuf,
        reason: String,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChangeSummary {
    pub modified: u32,
    pub created: u32,
    pub deleted: u32,
    pub binary: u32,
    pub redacted: u32,
    pub total_bytes_added: u64,
    pub total_bytes_removed: u64,
}

impl ChangeSummary {
    pub fn from_entries(entries: &[DiffEntry]) -> Self {
        let mut s = Self::default();
        for e in entries {
            match e {
                DiffEntry::Modified { before, after, .. } => {
                    s.modified += 1;
                    s.total_bytes_added += after.len() as u64;
                    s.total_bytes_removed += before.len() as u64;
                }
                DiffEntry::Created { after, .. } => {
                    s.created += 1;
                    s.total_bytes_added += after.len() as u64;
                }
                DiffEntry::Deleted { before, .. } => {
                    s.deleted += 1;
                    s.total_bytes_removed += before.len() as u64;
                }
                DiffEntry::Binary { .. } => s.binary += 1,
                DiffEntry::Redacted { .. } => s.redacted += 1,
            }
        }
        s
    }

    pub fn files_changed(&self) -> u32 {
        self.modified + self.created + self.deleted + self.binary + self.redacted
    }
}

/// Format a one-line summary of `entries`.
pub fn render_change_summary(summary: &ChangeSummary) -> String {
    format!(
        "{} files changed (+{}/-{}; {} new, {} deleted, {} binary, {} redacted)",
        summary.files_changed(),
        summary.total_bytes_added,
        summary.total_bytes_removed,
        summary.created,
        summary.deleted,
        summary.binary,
        summary.redacted,
    )
}

/// Render a unified-diff card for a single entry, capped at `max_lines`
/// lines to keep transcripts compact. Modified files render as
/// hunk-grouped unified diff so a change near the end of a file is still
/// visible when the cap is hit (the prior implementation streamed every
/// `Equal` line first, which made any change past `max_lines` invisible).
pub fn render_diff_card(entry: &DiffEntry, max_lines: usize) -> Vec<String> {
    match entry {
        DiffEntry::Modified {
            path,
            before,
            after,
        } => {
            let mut out = vec![format!("--- a/{}", path.display())];
            out.push(format!("+++ b/{}", path.display()));
            let diff = TextDiff::from_lines(before, after);
            // 3 lines of context per hunk is the GNU-diff default and gives
            // a reasonable signal-to-noise ratio inside the cap.
            let unified = diff.unified_diff();
            let mut emitted = 0usize;
            let mut total_change_lines = 0usize;
            let mut truncated = false;
            for hunk in unified.iter_hunks() {
                // Count the change lines in this hunk before deciding
                // whether to print it, so the truncation suffix can report
                // an accurate "more changes hidden" count.
                let hunk_changes: usize = hunk
                    .ops()
                    .iter()
                    .map(|op| {
                        diff.iter_changes(op)
                            .filter(|c| !matches!(c.tag(), ChangeTag::Equal))
                            .count()
                    })
                    .sum();
                total_change_lines += hunk_changes;
                if emitted >= max_lines {
                    truncated = true;
                    continue;
                }
                out.push(hunk.header().to_string());
                emitted += 1;
                for op in hunk.ops() {
                    for change in diff.iter_changes(op) {
                        if emitted >= max_lines {
                            truncated = true;
                            break;
                        }
                        let sigil = match change.tag() {
                            ChangeTag::Delete => "-",
                            ChangeTag::Insert => "+",
                            ChangeTag::Equal => " ",
                        };
                        let text = change.value();
                        let trimmed = text.strip_suffix('\n').unwrap_or(text);
                        out.push(format!("{}{}", sigil, trimmed));
                        emitted += 1;
                    }
                    if emitted >= max_lines {
                        truncated = true;
                        break;
                    }
                }
            }
            if truncated {
                out.push(format!(
                    "    …[truncated; {} change line(s) total]",
                    total_change_lines
                ));
            }
            out
        }
        DiffEntry::Created { path, after } => {
            let mut out = vec!["--- /dev/null".to_string()];
            out.push(format!("+++ b/{}", path.display()));
            for (i, line) in after.lines().enumerate() {
                if i >= max_lines {
                    out.push("    …[truncated]".to_string());
                    break;
                }
                out.push(format!("+{}", line));
            }
            out
        }
        DiffEntry::Deleted { path, before } => {
            let mut out = vec![format!("--- a/{}", path.display())];
            out.push("+++ /dev/null".to_string());
            for (i, line) in before.lines().enumerate() {
                if i >= max_lines {
                    out.push("    …[truncated]".to_string());
                    break;
                }
                out.push(format!("-{}", line));
            }
            out
        }
        DiffEntry::Binary { path } => {
            vec![format!(
                "Binary file {} changed (no diff shown)",
                path.display()
            )]
        }
        DiffEntry::Redacted { path, reason } => {
            vec![format!("{} (redacted: {})", path.display(), reason)]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // Modified diff renders unified-diff lines with +/- sigils.
    fn modified_renders_unified_diff() {
        let e = DiffEntry::Modified {
            path: PathBuf::from("src/lib.rs"),
            before: "old\nshared\n".to_string(),
            after: "new\nshared\n".to_string(),
        };
        let lines = render_diff_card(&e, 100);
        assert!(lines.iter().any(|l| l.starts_with("---")));
        assert!(lines.iter().any(|l| l.starts_with("+++")));
        assert!(lines.iter().any(|l| l == "-old"));
        assert!(lines.iter().any(|l| l == "+new"));
    }

    #[test]
    // A change near the end of a long file is still rendered when the line
    // cap is small. Regression test for the "iter_all_changes only shows
    // the first N lines" bug where late-file edits were invisible.
    fn change_near_end_of_file_is_visible() {
        let mut a = String::new();
        let mut b = String::new();
        for i in 0..200 {
            a.push_str(&format!("line {}\n", i));
            b.push_str(&format!("line {}\n", i));
        }
        // Edit the very last line.
        a.push_str("OLD-TAIL\n");
        b.push_str("NEW-TAIL\n");
        let e = DiffEntry::Modified {
            path: PathBuf::from("x.rs"),
            before: a,
            after: b,
        };
        let lines = render_diff_card(&e, 20);
        assert!(
            lines.iter().any(|l| l == "-OLD-TAIL"),
            "expected -OLD-TAIL in diff: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l == "+NEW-TAIL"),
            "expected +NEW-TAIL in diff: {lines:?}"
        );
    }

    #[test]
    // Truncation visibly indicates dropped lines.
    fn truncates_long_diffs() {
        let mut a = String::new();
        let mut b = String::new();
        for i in 0..50 {
            a.push_str(&format!("line {}\n", i));
            b.push_str(&format!("line-{}\n", i));
        }
        let e = DiffEntry::Modified {
            path: PathBuf::from("x.rs"),
            before: a,
            after: b,
        };
        let lines = render_diff_card(&e, 5);
        assert!(lines.iter().any(|l| l.contains("truncated")));
    }

    #[test]
    // Binary files render a one-liner placeholder.
    fn binary_rendered_safely() {
        let e = DiffEntry::Binary {
            path: PathBuf::from("blob.bin"),
        };
        let lines = render_diff_card(&e, 10);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("Binary"));
    }

    #[test]
    // Redacted files show why they were redacted instead of contents.
    fn redacted_files_show_reason() {
        let e = DiffEntry::Redacted {
            path: PathBuf::from(".env"),
            reason: "secret-like filename".to_string(),
        };
        let lines = render_diff_card(&e, 10);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("secret-like"));
    }

    #[test]
    // Summary aggregates counts and byte deltas.
    fn summary_counts_correctly() {
        let entries = vec![
            DiffEntry::Modified {
                path: PathBuf::from("a"),
                before: "abc".into(),
                after: "abcdef".into(),
            },
            DiffEntry::Created {
                path: PathBuf::from("b"),
                after: "x".into(),
            },
            DiffEntry::Binary {
                path: PathBuf::from("c"),
            },
        ];
        let s = ChangeSummary::from_entries(&entries);
        assert_eq!(s.modified, 1);
        assert_eq!(s.created, 1);
        assert_eq!(s.binary, 1);
        assert_eq!(s.total_bytes_added, 7);
        assert!(render_change_summary(&s).contains("3 files"));
    }
}
