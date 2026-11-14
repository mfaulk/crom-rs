//! Rust compiler / Cargo diagnostic extraction (M16B).
//!
//! Two paths: the preferred Cargo JSON path (`--message-format=json...`) and a
//! plain-text fallback that scans `error[E0308]: ...` / `--> file:line:col`
//! patterns. Both classify failures into [`DiagnosticKind`] using rustc error
//! codes first and message heuristics second. Parsing never panics on
//! malformed input — unparseable lines are simply skipped.

use std::path::PathBuf;
use std::sync::OnceLock;

use regex::Regex;
use serde::Deserialize;

use super::model::{
    DiagnosticKind, DiagnosticSeverity, DiagnosticSource, ParsedDiagnostic, RelatedDiagnostic,
    SuggestedFix,
};

/// Map a rustc error code (e.g. `E0308`) to a [`DiagnosticKind`]. Codes are
/// indicative, not authoritative; callers fall back to message heuristics.
pub fn kind_from_code(code: &str) -> Option<DiagnosticKind> {
    let k = match code {
        "E0308" => DiagnosticKind::TypeMismatch,
        "E0432" => DiagnosticKind::MissingImport,
        "E0433" => DiagnosticKind::UnresolvedName,
        "E0277" => DiagnosticKind::TraitBound,
        "E0599" => DiagnosticKind::MissingTraitImpl,
        "E0382" => DiagnosticKind::MoveError,
        "E0502" | "E0499" | "E0505" | "E0515" | "E0506" | "E0500" => DiagnosticKind::BorrowChecker,
        "E0596" => DiagnosticKind::MutabilityError,
        "E0603" => DiagnosticKind::VisibilityError,
        "E0412" | "E0425" | "E0405" | "E0531" => DiagnosticKind::UnresolvedName,
        "E0428" | "E0252" => DiagnosticKind::DuplicateDefinition,
        "E0106" | "E0621" | "E0623" => DiagnosticKind::Lifetime,
        "E0658" => DiagnosticKind::FeatureGate,
        _ => return None,
    };
    Some(k)
}

/// Classify a diagnostic from its (optional) code and message text. Used both
/// by the JSON and plain-text parsers.
pub fn classify(code: Option<&str>, message: &str) -> DiagnosticKind {
    if let Some(c) = code {
        if let Some(k) = kind_from_code(c) {
            return k;
        }
    }
    let m = message.to_ascii_lowercase();
    // Order matters: more specific phrases first.
    if m.contains("unresolved import") || (m.contains("cannot find") && m.contains("import")) {
        DiagnosticKind::MissingImport
    } else if m.contains("failed to resolve") && m.contains("crate") {
        DiagnosticKind::MissingCrate
    } else if m.contains("cannot find") || m.contains("not found in this scope") {
        DiagnosticKind::UnresolvedName
    } else if m.contains("expected one of")
        || (m.contains("expected `") && m.contains("unexpected"))
        || m.starts_with("expected ")
            && (m.contains("found `") && (m.contains('}') || m.contains(';') || m.contains(')')))
    {
        // Parser/syntax errors phrase themselves as "expected one of ..., found
        // ..." — check these before the type-mismatch heuristic, which also
        // keys on "expected"/"found".
        DiagnosticKind::SyntaxError
    } else if m.contains("mismatched types") || (m.contains("expected") && m.contains("found")) {
        DiagnosticKind::TypeMismatch
    } else if m.contains("the trait bound") || m.contains("is not satisfied") {
        DiagnosticKind::TraitBound
    } else if m.contains("no method named") || m.contains("no associated item") {
        DiagnosticKind::MissingTraitImpl
    } else if m.contains("borrow") {
        DiagnosticKind::BorrowChecker
    } else if m.contains("does not live long enough") || m.contains("lifetime") {
        DiagnosticKind::Lifetime
    } else if m.contains("use of moved value") || m.contains("value moved") {
        DiagnosticKind::MoveError
    } else if (m.contains("cannot borrow") && m.contains("as mutable"))
        || (m.contains("cannot assign") && m.contains("immutable"))
    {
        DiagnosticKind::MutabilityError
    } else if m.contains("is private") || (m.contains("private") && m.contains("module")) {
        DiagnosticKind::VisibilityError
    } else if m.contains("is defined multiple times") || m.contains("duplicate") {
        DiagnosticKind::DuplicateDefinition
    } else if m.contains("feature") && m.contains("is unstable") {
        DiagnosticKind::FeatureGate
    } else {
        DiagnosticKind::Unknown
    }
}

fn severity_from_level(level: &str) -> DiagnosticSeverity {
    match level {
        "error" | "error: internal compiler error" => DiagnosticSeverity::Error,
        "warning" => DiagnosticSeverity::Warning,
        "note" | "help" | "info" => DiagnosticSeverity::Info,
        _ => DiagnosticSeverity::Error,
    }
}

// --- Cargo JSON message records -------------------------------------------

#[derive(Debug, Deserialize)]
struct CargoLine {
    reason: Option<String>,
    message: Option<RustcMessage>,
}

#[derive(Debug, Deserialize)]
struct RustcMessage {
    message: String,
    #[serde(default)]
    code: Option<RustcCode>,
    level: String,
    #[serde(default)]
    spans: Vec<RustcSpan>,
    #[serde(default)]
    children: Vec<RustcChild>,
    #[serde(default)]
    rendered: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RustcCode {
    code: String,
}

#[derive(Debug, Deserialize)]
struct RustcSpan {
    file_name: String,
    line_start: u32,
    column_start: u32,
    line_end: Option<u32>,
    column_end: Option<u32>,
    #[serde(default)]
    is_primary: bool,
    #[serde(default)]
    suggested_replacement: Option<String>,
    #[serde(default)]
    suggestion_applicability: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RustcChild {
    message: String,
    level: String,
    #[serde(default)]
    spans: Vec<RustcSpan>,
}

/// Attempt to parse `text` as a stream of Cargo JSON message records. Returns
/// `None` if no JSON `compiler-message` lines were found (so the caller can
/// fall back to plain-text parsing).
pub fn parse_cargo_json(text: &str) -> Option<Vec<ParsedDiagnostic>> {
    let mut out = Vec::new();
    let mut saw_json = false;
    for line in text.lines() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        let parsed: CargoLine = match serde_json::from_str(line) {
            Ok(p) => p,
            Err(_) => continue,
        };
        // Only treat this as authoritative Cargo JSON output when the line is a
        // real Cargo message record (it carries a `reason`). An unrelated JSON
        // line such as `{}` or a test harness's own JSON deserializes into the
        // all-optional `CargoLine` with `reason = None`; counting those would
        // suppress the plain-text fallback and lose real `error[E…]` output.
        let Some(reason) = parsed.reason.as_deref() else {
            continue;
        };
        saw_json = true;
        if reason != "compiler-message" {
            continue;
        }
        let Some(msg) = parsed.message else { continue };
        // Skip pure notes that aren't standalone.
        if msg.level != "error" && msg.level != "warning" {
            continue;
        }
        out.push(rustc_message_to_diagnostic(msg));
    }
    if saw_json {
        Some(out)
    } else {
        None
    }
}

fn rustc_message_to_diagnostic(msg: RustcMessage) -> ParsedDiagnostic {
    let code = msg.code.as_ref().map(|c| c.code.clone());
    let kind = classify(code.as_deref(), &msg.message);
    let severity = severity_from_level(&msg.level);

    let primary = msg
        .spans
        .iter()
        .find(|s| s.is_primary)
        .or_else(|| msg.spans.first());

    let (file, line, column, end_line, end_column) = match primary {
        Some(s) => (
            Some(PathBuf::from(&s.file_name)),
            Some(s.line_start),
            Some(s.column_start),
            s.line_end,
            s.column_end,
        ),
        None => (None, None, None, None, None),
    };

    let mut related = Vec::new();
    let mut suggested_fixes = Vec::new();
    for child in &msg.children {
        let cspan = child.spans.first();
        related.push(RelatedDiagnostic {
            message: format!("{}: {}", child.level, child.message),
            file: cspan.map(|s| PathBuf::from(&s.file_name)),
            line: cspan.map(|s| s.line_start),
            column: cspan.map(|s| s.column_start),
        });
        for s in &child.spans {
            if let Some(repl) = &s.suggested_replacement {
                suggested_fixes.push(SuggestedFix {
                    message: child.message.clone(),
                    file: Some(PathBuf::from(&s.file_name)),
                    line: Some(s.line_start),
                    replacement: Some(repl.clone()),
                    applicability: s.suggestion_applicability.clone(),
                });
            }
        }
    }
    // Top-level span suggestions too.
    for s in &msg.spans {
        if let Some(repl) = &s.suggested_replacement {
            suggested_fixes.push(SuggestedFix {
                message: msg.message.clone(),
                file: Some(PathBuf::from(&s.file_name)),
                line: Some(s.line_start),
                replacement: Some(repl.clone()),
                applicability: s.suggestion_applicability.clone(),
            });
        }
    }

    let source = if code
        .as_deref()
        .map(|c| c.starts_with("clippy"))
        .unwrap_or(false)
        || msg.message.contains("clippy")
    {
        DiagnosticSource::Clippy
    } else {
        DiagnosticSource::Rustc
    };
    let kind = if source == DiagnosticSource::Clippy && severity == DiagnosticSeverity::Warning {
        DiagnosticKind::ClippyLint
    } else {
        kind
    };

    ParsedDiagnostic {
        source,
        severity,
        kind,
        code,
        message: msg.message,
        file,
        line,
        column,
        end_line,
        end_column,
        rendered_excerpt: msg.rendered,
        related,
        suggested_fixes,
    }
}

// --- Plain-text rustc fallback --------------------------------------------

/// Parse plain-text rustc / cargo output. Recognizes `error[E0308]: ...`,
/// `error: ...`, `warning: ...` headers and the following `--> file:line:col`
/// location line.
pub fn parse_plain_text(text: &str) -> Vec<ParsedDiagnostic> {
    static HEADER_RE: OnceLock<Regex> = OnceLock::new();
    static LOC_RE: OnceLock<Regex> = OnceLock::new();
    let header_re = HEADER_RE.get_or_init(|| {
        Regex::new(r"^(?P<level>error|warning)(?:\[(?P<code>[A-Za-z0-9]+)\])?:\s*(?P<msg>.*)$")
            .unwrap()
    });
    let loc_re = LOC_RE.get_or_init(|| {
        Regex::new(r"^\s*-->\s*(?P<file>[^:]+):(?P<line>\d+):(?P<col>\d+)").unwrap()
    });

    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if let Some(caps) = header_re.captures(line.trim_end()) {
            let level = caps.name("level").unwrap().as_str();
            // `error: aborting due to N previous errors` is a summary, skip it.
            let msg = caps.name("msg").unwrap().as_str().to_string();
            if msg.starts_with("aborting due to") || msg.starts_with("could not compile") {
                i += 1;
                continue;
            }
            let code = caps.name("code").map(|m| m.as_str().to_string());
            let severity = severity_from_level(level);
            let kind = classify(code.as_deref(), &msg);

            // Look ahead for a `-->` location line within the next few lines.
            let mut file = None;
            let mut line_no = None;
            let mut column = None;
            let mut rendered = vec![line.to_string()];
            let mut j = i + 1;
            // Look a bit beyond the header for the `-->` line; long multi-line
            // labels or macro-expansion notes can push it down.
            let look_until = (i + 16).min(lines.len());
            while j < look_until {
                rendered.push(lines[j].to_string());
                if let Some(loc) = loc_re.captures(lines[j]) {
                    file = Some(PathBuf::from(loc.name("file").unwrap().as_str().trim()));
                    line_no = loc.name("line").and_then(|m| m.as_str().parse().ok());
                    column = loc.name("col").and_then(|m| m.as_str().parse().ok());
                    break;
                }
                // Stop if we hit the next diagnostic header.
                if header_re.is_match(lines[j].trim_end()) {
                    rendered.pop();
                    break;
                }
                j += 1;
            }

            out.push(ParsedDiagnostic {
                source: DiagnosticSource::Rustc,
                severity,
                kind,
                code,
                message: msg,
                file,
                line: line_no,
                column,
                end_line: None,
                end_column: None,
                rendered_excerpt: Some(rendered.join("\n")),
                related: Vec::new(),
                suggested_fixes: Vec::new(),
            });
        }
        i += 1;
    }
    out
}

/// Parse Rust compiler diagnostics from combined command output, preferring
/// JSON when present and falling back to plain text otherwise.
pub fn parse(text: &str) -> Vec<ParsedDiagnostic> {
    match parse_cargo_json(text) {
        Some(v) if !v.is_empty() => v,
        // JSON present but no compiler-message records: still authoritative.
        Some(v) => v,
        None => parse_plain_text(text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_type_mismatch() {
        let line = r#"{"reason":"compiler-message","message":{"message":"mismatched types","code":{"code":"E0308"},"level":"error","spans":[{"file_name":"src/config.rs","line_start":74,"column_start":21,"line_end":74,"column_end":30,"is_primary":true}],"children":[{"message":"expected `OutputFormat`, found `String`","level":"note","spans":[]}],"rendered":"error[E0308]: mismatched types\n  --> src/config.rs:74:21"}}"#;
        let d = parse(line);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].kind, DiagnosticKind::TypeMismatch);
        assert_eq!(d[0].code.as_deref(), Some("E0308"));
        assert_eq!(
            d[0].file.as_ref().unwrap().to_str().unwrap(),
            "src/config.rs"
        );
        assert_eq!(d[0].line, Some(74));
        assert_eq!(d[0].related.len(), 1);
    }

    #[test]
    fn json_unresolved_import() {
        let line = r#"{"reason":"compiler-message","message":{"message":"unresolved import `foo`","code":{"code":"E0432"},"level":"error","spans":[{"file_name":"src/lib.rs","line_start":1,"column_start":5,"is_primary":true}],"children":[],"rendered":"error[E0432]"}}"#;
        let d = parse(line);
        assert_eq!(d[0].kind, DiagnosticKind::MissingImport);
    }

    #[test]
    fn plain_text_type_mismatch() {
        let log = "error[E0308]: mismatched types\n  --> src/foo.rs:42:17\n   |\n42 |     let x: u8 = \"s\";\n";
        let d = parse(log);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].kind, DiagnosticKind::TypeMismatch);
        assert_eq!(d[0].code.as_deref(), Some("E0308"));
        assert_eq!(d[0].line, Some(42));
        assert_eq!(d[0].column, Some(17));
    }

    #[test]
    fn plain_text_unresolved_import() {
        let log = "error[E0432]: unresolved import `foo`\n  --> src/lib.rs:1:5\n";
        let d = parse(log);
        assert_eq!(d[0].kind, DiagnosticKind::MissingImport);
        assert_eq!(d[0].file.as_ref().unwrap().to_str().unwrap(), "src/lib.rs");
    }

    #[test]
    fn plain_text_syntax_error() {
        let log = "error: expected one of `,` or `}`, found `x`\n  --> src/lib.rs:10:5\n";
        let d = parse(log);
        assert_eq!(d[0].kind, DiagnosticKind::SyntaxError);
    }

    #[test]
    fn skips_aborting_summary() {
        let log = "error[E0308]: mismatched types\n  --> a.rs:1:1\nerror: aborting due to 1 previous error\n";
        let d = parse(log);
        assert_eq!(d.len(), 1);
    }

    #[test]
    fn json_suggested_replacement_captured() {
        let line = r#"{"reason":"compiler-message","message":{"message":"help: add use","code":null,"level":"error","spans":[{"file_name":"src/lib.rs","line_start":1,"column_start":1,"is_primary":true,"suggested_replacement":"use std::collections::HashMap;\n","suggestion_applicability":"MachineApplicable"}],"children":[],"rendered":null}}"#;
        let d = parse(line);
        assert_eq!(d[0].suggested_fixes.len(), 1);
        assert_eq!(
            d[0].suggested_fixes[0].replacement.as_deref(),
            Some("use std::collections::HashMap;\n")
        );
    }

    #[test]
    fn malformed_json_does_not_panic() {
        let _ = parse("{not valid json\nerror: real thing\n  --> a.rs:1:1\n");
    }

    // A stray JSON line (e.g. `{}` from some other tool) must not suppress the
    // plain-text rustc fallback and lose the real error.
    #[test]
    fn stray_json_does_not_suppress_plain_text() {
        let log = "{}\nerror[E0308]: mismatched types\n  --> src/foo.rs:42:17\n";
        let d = parse(log);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].kind, DiagnosticKind::TypeMismatch);
    }

    #[test]
    fn classify_borrow_by_message() {
        assert_eq!(
            classify(None, "cannot borrow `x` as mutable more than once"),
            DiagnosticKind::BorrowChecker
        );
    }
}
