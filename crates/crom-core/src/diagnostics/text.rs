//! Small text-normalization helpers shared by the diagnostic parsers,
//! compactor, and fingerprinter. None of these ever panic on malformed input.

use std::sync::OnceLock;

use regex::Regex;

/// Strip ANSI/VT100 color and control escape sequences.
pub fn strip_ansi(input: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    // CSI sequences: ESC [ ... <final byte>. Also handle the bare ESC.
    let re = RE.get_or_init(|| Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]").unwrap());
    re.replace_all(input, "").into_owned()
}

/// Take the last `n` lines of `text`, preserving order.
pub fn last_lines(text: &str, n: usize) -> String {
    if n == 0 {
        return String::new();
    }
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

/// Truncate a string to at most `max_bytes`, appending a marker when content
/// was dropped. Respects UTF-8 char boundaries.
pub fn truncate_bytes(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let marker = "\n…[truncated]…";
    let budget = max_bytes.saturating_sub(marker.len());
    let mut end = budget.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{}", &text[..end], marker)
}

/// Normalize a message for fingerprinting: strip ANSI, collapse whitespace,
/// replace absolute/temp paths and volatile numbers so cosmetically different
/// runs of the *same* failure hash identically.
pub fn normalize_message(message: &str) -> String {
    static TEMP_RE: OnceLock<Regex> = OnceLock::new();
    static NUM_RE: OnceLock<Regex> = OnceLock::new();
    static WS_RE: OnceLock<Regex> = OnceLock::new();
    static HEX_RE: OnceLock<Regex> = OnceLock::new();

    let stripped = strip_ansi(message);
    // Replace common temp-dir prefixes with a stable token.
    let temp_re = TEMP_RE.get_or_init(|| {
        Regex::new(r"(?i)(/tmp|/var/folders|[A-Za-z]:\\Temp|/private/var/folders)[^\s:]*").unwrap()
    });
    let s = temp_re.replace_all(&stripped, "<TMP>").into_owned();
    // Replace 0x-style addresses and long hex blobs.
    let hex_re = HEX_RE.get_or_init(|| Regex::new(r"0x[0-9a-fA-F]+|\b[0-9a-f]{12,}\b").unwrap());
    let s = hex_re.replace_all(&s, "<HEX>").into_owned();
    // Replace standalone numbers (line numbers, counts) with a token so that a
    // shifting line count does not split the fingerprint.
    let num_re = NUM_RE.get_or_init(|| Regex::new(r"\b\d+\b").unwrap());
    let s = num_re.replace_all(&s, "<N>").into_owned();
    // Collapse whitespace.
    let ws_re = WS_RE.get_or_init(|| Regex::new(r"\s+").unwrap());
    ws_re.replace_all(s.trim(), " ").into_owned()
}

/// Normalize a command string for fingerprinting: collapse whitespace and trim.
/// Keeps the meaningful command shape (e.g. `cargo test parser::foo`).
pub fn normalize_command(command: &str) -> String {
    static WS_RE: OnceLock<Regex> = OnceLock::new();
    let ws_re = WS_RE.get_or_init(|| Regex::new(r"\s+").unwrap());
    ws_re.replace_all(command.trim(), " ").into_owned()
}

/// Make an absolute path relative to `cwd` when possible, for stable
/// fingerprints across differing temp roots. Returns the input unchanged if it
/// is already relative or not under `cwd`.
pub fn relativize(path: &std::path::Path, cwd: &std::path::Path) -> std::path::PathBuf {
    path.strip_prefix(cwd)
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_ansi_codes() {
        let s = "\x1b[1m\x1b[31merror\x1b[0m: boom";
        assert_eq!(strip_ansi(s), "error: boom");
    }

    #[test]
    fn truncate_respects_char_boundary() {
        let s = "héllo world this is long".repeat(10);
        let out = truncate_bytes(&s, 20);
        assert!(out.len() <= 20 + "\n…[truncated]…".len());
        assert!(out.ends_with("[truncated]…"));
    }

    #[test]
    fn normalize_message_stabilizes_paths_and_numbers() {
        let a = normalize_message("error at /tmp/abc123/src/foo.rs:42:1 value 0x7fff");
        let b = normalize_message("error at /tmp/zzz999/src/foo.rs:7:9 value 0x1234");
        assert_eq!(a, b);
    }

    #[test]
    fn last_lines_takes_tail() {
        assert_eq!(last_lines("a\nb\nc\nd", 2), "c\nd");
        assert_eq!(last_lines("", 2), "");
    }
}
