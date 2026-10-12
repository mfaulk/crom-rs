//! `cargo test` failure extraction (M16C).
//!
//! Parses libtest's human output: the `---- name stdout ----` blocks, the
//! `panicked at file:line:col` lines, `assertion `left == right` failed`
//! left/right snippets, the `failures:` name list, and the final
//! `test result: FAILED.` summary. Also recognizes bare panics. Never panics
//! on malformed input.

use std::path::PathBuf;
use std::sync::OnceLock;

use regex::Regex;

use super::model::{
    DiagnosticKind, DiagnosticSeverity, DiagnosticSource, FailingTest, ParsedDiagnostic,
};

/// Result of parsing `cargo test` output.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TestParse {
    pub failing_tests: Vec<FailingTest>,
    pub diagnostics: Vec<ParsedDiagnostic>,
    /// `(passed, failed, ignored)` from the result summary, when present.
    pub summary_counts: Option<(u32, u32, u32)>,
}

fn panic_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"thread '(?P<thread>[^']*)' panicked at (?P<file>[^:\n]+):(?P<line>\d+):(?P<col>\d+):",
        )
        .unwrap()
    })
}

/// Classify a panic message into a finer test-failure kind.
fn classify_panic(message: &str) -> DiagnosticKind {
    let m = message.to_ascii_lowercase();
    if m.contains("assertion") && m.contains("failed") {
        DiagnosticKind::TestAssertionFailure
    } else if m.contains("snapshot") {
        DiagnosticKind::SnapshotMismatch
    } else {
        DiagnosticKind::TestPanic
    }
}

/// Parse `cargo test` output into failing tests + diagnostics.
pub fn parse(text: &str) -> TestParse {
    static STDOUT_HDR: OnceLock<Regex> = OnceLock::new();
    static SUMMARY_RE: OnceLock<Regex> = OnceLock::new();
    static LEFT_RE: OnceLock<Regex> = OnceLock::new();
    static RIGHT_RE: OnceLock<Regex> = OnceLock::new();

    let stdout_hdr = STDOUT_HDR
        .get_or_init(|| Regex::new(r"^----\s+(?P<name>\S+)\s+stdout\s+----\s*$").unwrap());
    let summary_re = SUMMARY_RE.get_or_init(|| {
        Regex::new(r"test result:\s+(?P<res>\w+)\.\s+(?P<passed>\d+) passed; (?P<failed>\d+) failed; (?P<ignored>\d+) ignored")
            .unwrap()
    });
    let left_re = LEFT_RE.get_or_init(|| Regex::new(r"^\s*left:\s*(?P<v>.*)$").unwrap());
    let right_re = RIGHT_RE.get_or_init(|| Regex::new(r"^\s*right:\s*(?P<v>.*)$").unwrap());

    let lines: Vec<&str> = text.lines().collect();
    let mut result = TestParse::default();

    // Pass 1: per-test stdout blocks (`---- name stdout ----` ... until blank
    // line or next header).
    let mut i = 0;
    while i < lines.len() {
        if let Some(caps) = stdout_hdr.captures(lines[i].trim_end()) {
            let name = caps.name("name").unwrap().as_str().to_string();
            let mut block: Vec<&str> = Vec::new();
            let mut j = i + 1;
            while j < lines.len() {
                let l = lines[j];
                if stdout_hdr.is_match(l.trim_end()) {
                    break;
                }
                if l.starts_with("failures:") || summary_re.is_match(l) {
                    break;
                }
                block.push(l);
                j += 1;
            }
            let ft = build_failing_test(name, &block);
            result.diagnostics.push(failing_test_to_diagnostic(&ft));
            result.failing_tests.push(ft);
            i = j;
            continue;
        }
        i += 1;
    }

    // Pass 2: if no stdout blocks were found, look for bare panics.
    if result.failing_tests.is_empty() {
        for caps in panic_re().captures_iter(text) {
            let file = caps.name("file").map(|m| PathBuf::from(m.as_str()));
            let line = caps.name("line").and_then(|m| m.as_str().parse().ok());
            // Grab the message on the next line(s).
            let thread = caps.name("thread").map(|m| m.as_str()).unwrap_or("");
            let msg = panic_message_after(text, caps.get(0).unwrap().end());
            let kind = classify_panic(&msg);
            let ft = FailingTest {
                name: if thread.is_empty() {
                    "<panic>".into()
                } else {
                    thread.to_string()
                },
                file: file.clone(),
                line,
                message: Some(msg.clone()),
                assertion_left: None,
                assertion_right: None,
                output_excerpt: super::text::truncate_bytes(&msg, 800),
            };
            result.diagnostics.push(ParsedDiagnostic {
                source: DiagnosticSource::Panic,
                severity: DiagnosticSeverity::Failure,
                kind,
                code: None,
                message: msg,
                file,
                line,
                column: None,
                end_line: None,
                end_column: None,
                rendered_excerpt: None,
                related: Vec::new(),
                suggested_fixes: Vec::new(),
            });
            result.failing_tests.push(ft);
        }
    }

    // Summary counts.
    for l in &lines {
        if let Some(caps) = summary_re.captures(l) {
            let passed = caps
                .name("passed")
                .and_then(|m| m.as_str().parse().ok())
                .unwrap_or(0);
            let failed = caps
                .name("failed")
                .and_then(|m| m.as_str().parse().ok())
                .unwrap_or(0);
            let ignored = caps
                .name("ignored")
                .and_then(|m| m.as_str().parse().ok())
                .unwrap_or(0);
            result.summary_counts = Some((passed, failed, ignored));
        }
    }

    let _ = (left_re, right_re); // used inside build_failing_test
    result
}

fn panic_message_after(text: &str, offset: usize) -> String {
    let rest = &text[offset..];
    // The message is typically on the next line.
    rest.lines()
        .map(|l| l.trim())
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string()
}

fn build_failing_test(name: String, block: &[&str]) -> FailingTest {
    static LEFT_RE: OnceLock<Regex> = OnceLock::new();
    static RIGHT_RE: OnceLock<Regex> = OnceLock::new();
    let left_re = LEFT_RE.get_or_init(|| Regex::new(r"^\s*left:\s*(?P<v>.*)$").unwrap());
    let right_re = RIGHT_RE.get_or_init(|| Regex::new(r"^\s*right:\s*(?P<v>.*)$").unwrap());

    let joined = block.join("\n");
    let mut file = None;
    let mut line = None;
    let mut message = None;

    if let Some(caps) = panic_re().captures(&joined) {
        file = caps.name("file").map(|m| PathBuf::from(m.as_str()));
        line = caps.name("line").and_then(|m| m.as_str().parse().ok());
        // The line after `panicked at ...:` is the message.
        let end = caps.get(0).unwrap().end();
        let after = &joined[end..];
        message = after
            .lines()
            .map(|l| l.trim())
            .find(|l| !l.is_empty())
            .map(|s| s.to_string());
    }

    let assertion_left = block.iter().find_map(|l| {
        left_re
            .captures(l)
            .map(|c| c.name("v").unwrap().as_str().trim().to_string())
    });
    let assertion_right = block.iter().find_map(|l| {
        right_re
            .captures(l)
            .map(|c| c.name("v").unwrap().as_str().trim().to_string())
    });

    FailingTest {
        name,
        file,
        line,
        message,
        assertion_left,
        assertion_right,
        output_excerpt: super::text::truncate_bytes(joined.trim(), 1200),
    }
}

fn failing_test_to_diagnostic(ft: &FailingTest) -> ParsedDiagnostic {
    let msg = ft
        .message
        .clone()
        .unwrap_or_else(|| format!("test {} failed", ft.name));
    let kind = classify_panic(&msg);
    ParsedDiagnostic {
        source: DiagnosticSource::CargoTest,
        severity: DiagnosticSeverity::Failure,
        kind,
        code: None,
        message: format!("test `{}` failed: {}", ft.name, msg),
        file: ft.file.clone(),
        line: ft.line,
        column: None,
        end_line: None,
        end_column: None,
        rendered_excerpt: Some(ft.output_excerpt.clone()),
        related: Vec::new(),
        suggested_fixes: Vec::new(),
    }
}

/// Quick check: does this output look like `cargo test` output at all?
pub fn looks_like_test_output(text: &str) -> bool {
    text.contains("test result:")
        || text.contains("running ") && text.contains(" test")
        || text.contains("---- ") && text.contains(" stdout ----")
        || panic_re().is_match(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ASSERTION: &str = r#"
running 13 tests
test parser::tests::ok ... ok
test parser::tests::parses_json_format ... FAILED

failures:

---- parser::tests::parses_json_format stdout ----
thread 'parser::tests::parses_json_format' panicked at src/parser.rs:88:9:
assertion `left == right` failed
  left: 1
 right: 2
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace

failures:
    parser::tests::parses_json_format

test result: FAILED. 12 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out
"#;

    #[test]
    fn parses_assertion_failure() {
        let p = parse(ASSERTION);
        assert_eq!(p.failing_tests.len(), 1);
        let ft = &p.failing_tests[0];
        assert_eq!(ft.name, "parser::tests::parses_json_format");
        assert_eq!(ft.file.as_ref().unwrap().to_str().unwrap(), "src/parser.rs");
        assert_eq!(ft.line, Some(88));
        assert_eq!(ft.assertion_left.as_deref(), Some("1"));
        assert_eq!(ft.assertion_right.as_deref(), Some("2"));
        assert_eq!(p.summary_counts, Some((12, 1, 0)));
        assert_eq!(p.diagnostics[0].kind, DiagnosticKind::TestAssertionFailure);
    }

    const PANIC: &str = r#"
---- foo::bar stdout ----
thread 'foo::bar' panicked at src/lib.rs:12:5:
called `Option::unwrap()` on a `None` value

failures:
    foo::bar

test result: FAILED. 0 passed; 1 failed; 0 ignored
"#;

    #[test]
    fn parses_panic() {
        let p = parse(PANIC);
        assert_eq!(p.failing_tests.len(), 1);
        let ft = &p.failing_tests[0];
        assert_eq!(ft.line, Some(12));
        assert!(ft.message.as_ref().unwrap().contains("unwrap"));
        assert_eq!(p.diagnostics[0].kind, DiagnosticKind::TestPanic);
    }

    const MULTI: &str = r#"
failures:

---- a::one stdout ----
thread 'a::one' panicked at src/a.rs:1:1:
assertion `left == right` failed
  left: true
 right: false

---- a::two stdout ----
thread 'a::two' panicked at src/a.rs:9:1:
boom

failures:
    a::one
    a::two

test result: FAILED. 3 passed; 2 failed; 0 ignored
"#;

    #[test]
    fn parses_multiple_failures() {
        let p = parse(MULTI);
        assert_eq!(p.failing_tests.len(), 2);
        assert_eq!(p.failing_tests[0].name, "a::one");
        assert_eq!(p.failing_tests[1].name, "a::two");
        assert_eq!(p.summary_counts, Some((3, 2, 0)));
    }

    #[test]
    fn bare_panic_without_test_block() {
        let log = "thread 'main' panicked at src/main.rs:3:9:\nindex out of bounds: the len is 0 but the index is 5\n";
        let p = parse(log);
        assert_eq!(p.failing_tests.len(), 1);
        assert_eq!(p.failing_tests[0].line, Some(3));
    }

    #[test]
    fn truncated_output_does_not_panic() {
        let _ = parse("---- x stdout ----\nthread 'x' panicked at");
    }
}
