use similar::TextDiff;

/// Render a unified diff for `path` between `old` and `new` text. The output
/// is deterministic for the same inputs.
pub fn render_unified_diff(path: &str, old: &str, new: &str) -> String {
    let diff = TextDiff::from_lines(old, new);
    let mut out = String::new();
    out.push_str(&format!("--- a/{path}\n+++ b/{path}\n"));
    let mut unified = diff.unified_diff();
    unified.context_radius(3);
    out.push_str(&unified.to_string());
    out
}

/// Render a "create" diff (everything is an addition).
pub fn render_unified_diff_for_create(path: &str, new: &str) -> String {
    let diff = TextDiff::from_lines("", new);
    let mut out = String::new();
    out.push_str(&format!("--- /dev/null\n+++ b/{path}\n"));
    let mut unified = diff.unified_diff();
    unified.context_radius(3);
    out.push_str(&unified.to_string());
    out
}

/// Count `+` and `-` lines in a unified diff body (excluding the file
/// header markers `+++` and `---`).
pub fn summarize_diff(unified: &str) -> (usize, usize) {
    let mut added = 0;
    let mut removed = 0;
    for line in unified.lines() {
        if line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        if line.starts_with('+') {
            added += 1;
        } else if line.starts_with('-') {
            removed += 1;
        }
    }
    (added, removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// summarize_diff counts added and removed lines, ignoring header markers.
    #[test]
    fn summarize_ignores_header_markers() {
        let diff = "--- a/x\n+++ b/x\n@@\n-old\n+new\n+more\n";
        let (a, r) = summarize_diff(diff);
        assert_eq!(a, 2);
        assert_eq!(r, 1);
    }

    /// render_unified_diff is stable across calls with the same input.
    #[test]
    fn unified_diff_stable() {
        let a = render_unified_diff("x", "a\nb\n", "a\nb\nc\n");
        let b = render_unified_diff("x", "a\nb\n", "a\nb\nc\n");
        assert_eq!(a, b);
    }
}
