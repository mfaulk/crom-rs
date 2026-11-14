//! Behavior-level analysis (M15E).
//!
//! Beyond pass/fail, the harness derives *behavior metrics* from the event
//! timeline, the tool-call log, and the final patch. These describe **how** the
//! agent worked: did it read before writing, run tests after editing, recover
//! from a compiler error, delete tests, make broad blind edits, and so on.
//!
//! Metrics use `Option<_>` where the data is genuinely unavailable (e.g.
//! checkpoint events are not surfaced through the SDK event stream) rather than
//! fabricating a value. Behavior rules ([`crate::schema::BehaviorRules`]) are
//! evaluated against these metrics and surface as quality signals in reports.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crom_sdk::SdkEvent;

use crate::fixture::{looks_like_test_path, PatchInfo};
use crate::schema::BehaviorRules;
use crate::task::EvalTask;

/// Behavior metrics derived from a single run.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BehaviorMetrics {
    pub read_before_write: Option<bool>,
    pub grep_before_edit: Option<bool>,
    pub test_after_edit: Option<bool>,
    pub compiler_error_recovered: Option<bool>,
    pub same_error_repeated: Option<bool>,
    pub plan_submitted: Option<bool>,
    pub plan_updated_after_failure: Option<bool>,
    /// Not currently surfaced through the SDK event stream → always `None`.
    pub checkpoint_before_mutation: Option<bool>,
    pub forbidden_tool_attempted: Option<bool>,
    pub large_blind_edit: Option<bool>,
    pub deleted_tests: Option<bool>,
    pub weakened_assertions: Option<bool>,
    pub cargo_toml_changed: Option<bool>,
    /// Derived from the patch's changed files. Note: the default fixture
    /// pipeline excludes `Cargo.lock` from patch generation (cargo regenerates
    /// it as a build artifact), so for tasks whose verification runs cargo this
    /// is effectively always `false`. It is meaningful only when `Cargo.lock` is
    /// tracked in the fixture and surfaced in the patch.
    pub cargo_lock_changed: Option<bool>,
    pub validation_command_count: Option<u64>,
    pub edit_count: Option<u64>,
    pub unique_files_read: Option<u64>,
    pub unique_files_edited: Option<u64>,
}

const MUTATING_TOOLS: &[&str] = &["Edit", "Write"];
const SEARCH_TOOLS: &[&str] = &["Grep", "Glob"];
const LARGE_EDIT_CHURN: usize = 80;

fn tool_name(e: &SdkEvent) -> Option<&str> {
    match e {
        SdkEvent::ToolCallStarted(ev) => Some(ev.tool_name.as_str()),
        _ => None,
    }
}

fn tool_file(e: &SdkEvent) -> Option<&str> {
    match e {
        SdkEvent::ToolCallStarted(ev) => ev.input.get("file_path").and_then(|v| v.as_str()),
        _ => None,
    }
}

fn bash_text(e: &SdkEvent) -> Option<&str> {
    match e {
        SdkEvent::ToolCallStarted(ev) if ev.tool_name == "Bash" => {
            ev.input.get("command").and_then(|v| v.as_str())
        }
        _ => None,
    }
}

fn is_validation_command(cmd: &str) -> bool {
    cmd.contains("cargo")
        || cmd.contains("test")
        || cmd.contains("clippy")
        || cmd.contains("check")
        || cmd.contains("make")
}

impl BehaviorMetrics {
    /// Derive metrics from the event stream, the final patch, and the task.
    pub fn derive(events: &[SdkEvent], patch: &PatchInfo, task: &EvalTask) -> Self {
        let mut reads_seen: BTreeSet<String> = BTreeSet::new();
        let mut edits_seen: BTreeSet<String> = BTreeSet::new();
        let mut first_read_idx: Option<usize> = None;
        let mut first_edit_idx: Option<usize> = None;
        let mut first_search_idx: Option<usize> = None;
        let mut last_edit_idx: Option<usize> = None;
        let mut edit_count = 0u64;
        let mut validation_count = 0u64;
        let mut blind_edit = false;
        let mut forbidden_attempt = false;

        let allowed: Option<BTreeSet<&str>> = if task.allowed_tools.is_empty() {
            None
        } else {
            Some(task.allowed_tools.iter().map(String::as_str).collect())
        };

        for (i, e) in events.iter().enumerate() {
            if let Some(name) = tool_name(e) {
                if let Some(set) = &allowed {
                    if !set.contains(name) && name != "TodoWrite" {
                        forbidden_attempt = true;
                    }
                }
                if name == "Read" {
                    first_read_idx.get_or_insert(i);
                    if let Some(f) = tool_file(e) {
                        reads_seen.insert(f.to_string());
                    }
                } else if SEARCH_TOOLS.contains(&name) {
                    first_search_idx.get_or_insert(i);
                } else if MUTATING_TOOLS.contains(&name) {
                    first_edit_idx.get_or_insert(i);
                    last_edit_idx = Some(i);
                    edit_count += 1;
                    if let Some(f) = tool_file(e) {
                        if !reads_seen.contains(f) {
                            blind_edit = true;
                        }
                        edits_seen.insert(f.to_string());
                    }
                }
            }
            if let Some(cmd) = bash_text(e) {
                if is_validation_command(cmd) {
                    validation_count += 1;
                }
            }
        }

        let has_edits = first_edit_idx.is_some();

        let read_before_write = first_edit_idx.map(|edit| match first_read_idx {
            Some(read) => read < edit,
            None => false,
        });
        let grep_before_edit = first_edit_idx.map(|edit| match first_search_idx {
            Some(search) => search < edit,
            None => false,
        });
        let test_after_edit = last_edit_idx.map(|edit| {
            events
                .iter()
                .enumerate()
                .any(|(i, e)| i > edit && bash_text(e).is_some_and(is_validation_command))
        });

        let churn = patch.insertions + patch.deletions;
        let large_blind_edit = if has_edits {
            Some(blind_edit && churn > LARGE_EDIT_CHURN)
        } else {
            None
        };

        let plan_events = events
            .iter()
            .filter(|e| matches!(e, SdkEvent::PlanCreated(_)))
            .count();
        let plan_submitted = Some(plan_events > 0);
        let plan_updated_after_failure = if plan_events > 0 {
            Some(plan_events > 1)
        } else {
            None
        };

        Self {
            read_before_write,
            grep_before_edit,
            test_after_edit,
            compiler_error_recovered: Some(recovered_from_failure(events)),
            same_error_repeated: same_error_repeated(events),
            plan_submitted,
            plan_updated_after_failure,
            checkpoint_before_mutation: None,
            forbidden_tool_attempted: allowed.as_ref().map(|_| forbidden_attempt),
            large_blind_edit,
            deleted_tests: Some(patch.deleted_files.iter().any(|f| looks_like_test_path(f))),
            weakened_assertions: Some(weakened_assertions(patch)),
            cargo_toml_changed: Some(path_changed(patch, "Cargo.toml")),
            cargo_lock_changed: Some(path_changed(patch, "Cargo.lock")),
            validation_command_count: Some(validation_count),
            edit_count: Some(edit_count),
            unique_files_read: Some(reads_seen.len() as u64),
            unique_files_edited: Some(edits_seen.len() as u64),
        }
    }
}

fn path_changed(patch: &PatchInfo, suffix: &str) -> bool {
    patch
        .changed_files
        .iter()
        .any(|f| f == suffix || f.ends_with(&format!("/{suffix}")))
}

/// Does a diff line (sans leading `+`/`-`) reference an assertion?
fn is_assertion_line(content: &str) -> bool {
    content.contains("assert!")
        || content.contains("assert_eq!")
        || content.contains("assert_ne!")
        || content.contains("should_panic")
        || content.contains(".expect(")
        || content.contains("debug_assert")
}

/// Detect assertions weakened in **test context** only.
///
/// The raw "removed assert line" heuristic false-positives on clippy/refactor
/// cleanups that drop a redundant `.expect(...)` or `assert!(true)` in
/// production code. To stay advisory but accurate we:
///
/// 1. Scope counting to test context — only hunks whose file is a test path
///    (`tests/`, `*_test.rs`, …) or whose surrounding diff region is inside a
///    `#[cfg(test)]` module / `#[test]` function. The file is considered a test
///    path if EITHER the new (`+++`) or old (`---`) path looks like a test path,
///    so a deleted test file (`--- a/tests/foo.rs` → `+++ /dev/null`) still
///    counts as test context.
/// 2. Use the **net** count: `removed_asserts - added_asserts > 0`, so moving
///    or rewriting an assertion (net zero) does not trip the signal.
///
/// LIMITATION (diff-only analysis): this function sees only `patch.unified_diff`,
/// not the full post-change file contents. In-hunk `#[cfg(test)]` / `#[test]`
/// marker detection is therefore best-effort: when the marker for a production
/// file's test module/fn falls OUTSIDE the changed hunk's context lines, that
/// removed assertion is intentionally not flagged. This is an advisory metric
/// deliberately biased toward avoiding the false-POSITIVES that motivated #47;
/// missing some true-positives in unmarked production test regions is the
/// accepted trade-off. File-path scoping (which is reliable for whole test
/// files, including deletions) remains the primary signal.
fn weakened_assertions(patch: &PatchInfo) -> bool {
    let mut current_file_is_test = false;
    // Whether the current hunk's context puts us inside a `#[cfg(test)]` module
    // or `#[test]` fn. Approximated from markers seen in this file's diff so
    // far (context/added/removed lines), reset at each new file.
    let mut in_test_region = false;
    let mut net: i64 = 0;

    for line in patch.unified_diff.lines() {
        // Old-file header: `--- a/path/to/file.rs` (or `/dev/null` for an add).
        // Seed test-ness from the old path so a deletion (`+++ /dev/null`) keeps
        // its test context. Resets per-file state, which the `+++` header
        // immediately follows in a unified diff.
        if let Some(path) = line.strip_prefix("--- ") {
            let path = path.strip_prefix("a/").unwrap_or(path);
            current_file_is_test = looks_like_test_path(path);
            in_test_region = false;
            continue;
        }
        // New file header: `+++ b/path/to/file.rs` (or `/dev/null` for a
        // deletion). Keep test-ness if EITHER the old or new path is a test path.
        if let Some(path) = line.strip_prefix("+++ ") {
            let path = path.strip_prefix("b/").unwrap_or(path);
            current_file_is_test = current_file_is_test || looks_like_test_path(path);
            continue;
        }
        // Hunk header resets the region heuristic (we can no longer assume the
        // previously seen `#[cfg(test)]` marker applies across a gap).
        if line.starts_with("@@") {
            in_test_region = false;
            continue;
        }

        // Content of the line without the diff marker (` `, `+`, or `-`).
        let content = match line.chars().next() {
            Some('+') | Some('-') | Some(' ') => &line[1..],
            _ => line,
        };

        // Track test-region markers from context/added/removed lines.
        if content.contains("#[cfg(test)]") || content.contains("#[test]") {
            in_test_region = true;
        }

        let in_test_context = current_file_is_test || in_test_region;
        if !in_test_context {
            continue;
        }

        if line.starts_with('-') && is_assertion_line(content) {
            net += 1;
        } else if line.starts_with('+') && is_assertion_line(content) {
            net -= 1;
        }
    }

    net > 0
}

fn recovered_from_failure(events: &[SdkEvent]) -> bool {
    let mut saw_failure = false;
    for ev in events {
        match ev {
            SdkEvent::ToolCallCompleted(c) if c.is_error => saw_failure = true,
            SdkEvent::ToolCallStarted(s)
                if saw_failure && MUTATING_TOOLS.contains(&s.tool_name.as_str()) =>
            {
                return true;
            }
            _ => {}
        }
    }
    false
}

/// `Some(true)` when the same Bash command text ran 3+ times (a loop). `None`
/// when no Bash commands ran.
fn same_error_repeated(events: &[SdkEvent]) -> Option<bool> {
    use std::collections::HashMap;
    let mut seen: HashMap<&str, usize> = HashMap::new();
    for e in events {
        if let Some(cmd) = bash_text(e) {
            *seen.entry(cmd).or_insert(0) += 1;
        }
    }
    if seen.is_empty() {
        return None;
    }
    Some(seen.values().any(|&n| n >= 3))
}

/// Result of evaluating a single behavior rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BehaviorRuleResult {
    pub rule: String,
    pub satisfied: bool,
    pub detail: String,
}

/// Evaluate behavior rules against derived metrics. Returns one result per
/// enabled rule. Rules with unavailable data are reported as satisfied (we do
/// not penalise on missing signal).
pub fn evaluate_rules(rules: &BehaviorRules, m: &BehaviorMetrics) -> Vec<BehaviorRuleResult> {
    let mut out = Vec::new();
    if rules.require_read_before_write {
        let ok = m.read_before_write.unwrap_or(true);
        out.push(BehaviorRuleResult {
            rule: "require_read_before_write".into(),
            satisfied: ok,
            detail: format!("read_before_write = {:?}", m.read_before_write),
        });
    }
    if rules.require_validation_after_edit {
        let ok = m.test_after_edit.unwrap_or(true);
        out.push(BehaviorRuleResult {
            rule: "require_validation_after_edit".into(),
            satisfied: ok,
            detail: format!("test_after_edit = {:?}", m.test_after_edit),
        });
    }
    if rules.penalize_test_deletion {
        let deleted = m.deleted_tests.unwrap_or(false);
        out.push(BehaviorRuleResult {
            rule: "penalize_test_deletion".into(),
            satisfied: !deleted,
            detail: format!("deleted_tests = {deleted}"),
        });
    }
    if rules.penalize_large_blind_edit {
        let blind = m.large_blind_edit.unwrap_or(false);
        out.push(BehaviorRuleResult {
            rule: "penalize_large_blind_edit".into(),
            satisfied: !blind,
            detail: format!("large_blind_edit = {blind}"),
        });
    }
    if rules.penalize_lockfile_change {
        let lock = m.cargo_lock_changed.unwrap_or(false);
        out.push(BehaviorRuleResult {
            rule: "penalize_lockfile_change".into(),
            satisfied: !lock,
            detail: format!("cargo_lock_changed = {lock}"),
        });
    }
    out
}

/// Human-facing warnings derived from metrics + rule results, used in reports
/// and triage. Empty when the run is clean.
pub fn warnings(m: &BehaviorMetrics, rule_results: &[BehaviorRuleResult]) -> Vec<String> {
    let mut w = Vec::new();
    if m.deleted_tests == Some(true) {
        w.push("agent deleted test file(s)".into());
    }
    if m.weakened_assertions == Some(true) {
        w.push("agent removed assertions from the diff".into());
    }
    if m.large_blind_edit == Some(true) {
        w.push("agent made a large edit to a file it never read".into());
    }
    if m.same_error_repeated == Some(true) {
        w.push("agent repeated the same command 3+ times (possible loop)".into());
    }
    if m.read_before_write == Some(false) {
        w.push("agent edited before reading".into());
    }
    if m.forbidden_tool_attempted == Some(true) {
        w.push("agent attempted a disallowed tool".into());
    }
    if m.cargo_lock_changed == Some(true) {
        w.push("agent changed Cargo.lock".into());
    }
    for r in rule_results {
        if !r.satisfied {
            w.push(format!("behavior rule violated: {} ({})", r.rule, r.detail));
        }
    }
    w
}

#[cfg(test)]
mod tests {
    use super::*;
    use crom_sdk::event::{ToolCallCompletedEvent, ToolCallStartedEvent};
    use crom_sdk::RunId;

    fn started(tool: &str, input: serde_json::Value) -> SdkEvent {
        SdkEvent::ToolCallStarted(ToolCallStartedEvent {
            run_id: RunId::new(),
            tool_name: tool.to_string(),
            tool_use_id: "t".into(),
            input,
        })
    }

    fn completed(tool: &str, is_error: bool) -> SdkEvent {
        SdkEvent::ToolCallCompleted(ToolCallCompletedEvent {
            run_id: RunId::new(),
            tool_name: tool.to_string(),
            tool_use_id: "t".into(),
            is_error,
            duration_ms: 1,
        })
    }

    fn task() -> EvalTask {
        EvalTask::minimal("t", "t", "p", crate::task::OracleSpec::default(), ".")
    }

    #[test]
    fn read_before_write_true_and_false() {
        let good = vec![
            started("Read", serde_json::json!({"file_path": "src/lib.rs"})),
            started(
                "Edit",
                serde_json::json!({"file_path": "src/lib.rs", "old_string":"a","new_string":"b"}),
            ),
        ];
        let m = BehaviorMetrics::derive(&good, &PatchInfo::default(), &task());
        assert_eq!(m.read_before_write, Some(true));
        assert_eq!(m.unique_files_read, Some(1));
        assert_eq!(m.edit_count, Some(1));

        let bad = vec![started(
            "Write",
            serde_json::json!({"file_path": "src/lib.rs", "content":"x"}),
        )];
        let m2 = BehaviorMetrics::derive(&bad, &PatchInfo::default(), &task());
        assert_eq!(m2.read_before_write, Some(false));
    }

    #[test]
    fn test_after_edit_detected() {
        let events = vec![
            started("Edit", serde_json::json!({"file_path": "src/lib.rs"})),
            started("Bash", serde_json::json!({"command": "cargo test"})),
        ];
        let m = BehaviorMetrics::derive(&events, &PatchInfo::default(), &task());
        assert_eq!(m.test_after_edit, Some(true));
        assert_eq!(m.validation_command_count, Some(1));
    }

    #[test]
    fn deleted_tests_and_lock_and_toml() {
        let patch = PatchInfo {
            changed_files: vec![
                "Cargo.toml".into(),
                "Cargo.lock".into(),
                "tests/it.rs".into(),
            ],
            deleted_files: vec!["tests/it.rs".into()],
            ..Default::default()
        };
        let m = BehaviorMetrics::derive(&[], &patch, &task());
        assert_eq!(m.deleted_tests, Some(true));
        assert_eq!(m.cargo_toml_changed, Some(true));
        assert_eq!(m.cargo_lock_changed, Some(true));
    }

    #[test]
    fn large_blind_edit_detected() {
        let events = vec![started(
            "Write",
            serde_json::json!({"file_path": "src/lib.rs", "content":"x"}),
        )];
        let patch = PatchInfo {
            changed_files: vec!["src/lib.rs".into()],
            insertions: 100,
            deletions: 0,
            ..Default::default()
        };
        let m = BehaviorMetrics::derive(&events, &patch, &task());
        assert_eq!(m.large_blind_edit, Some(true));
    }

    #[test]
    fn weakened_assertions_in_test_file_flagged() {
        // Removing an assert_eq! from a tests/ file IS flagged.
        let patch = PatchInfo {
            unified_diff: "--- a/tests/foo.rs\n+++ b/tests/foo.rs\n@@ -1,3 +1,2 @@\n     let x = compute();\n-    assert_eq!(x, 1);\n+    // removed\n".into(),
            changed_files: vec!["tests/foo.rs".into()],
            ..Default::default()
        };
        let m = BehaviorMetrics::derive(&[], &patch, &task());
        assert_eq!(m.weakened_assertions, Some(true));
    }

    #[test]
    fn weakened_assertions_in_cfg_test_region_flagged() {
        // Removing an assert from inside a `#[cfg(test)]` module IS flagged,
        // even when the file path is not itself a test path.
        let patch = PatchInfo {
            unified_diff: "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -10,5 +10,4 @@ mod tests {\n     #[cfg(test)]\n     fn it_works() {\n-        assert_eq!(2 + 2, 4);\n+        // removed\n     }\n".into(),
            changed_files: vec!["src/lib.rs".into()],
            ..Default::default()
        };
        let m = BehaviorMetrics::derive(&[], &patch, &task());
        assert_eq!(m.weakened_assertions, Some(true));
    }

    #[test]
    fn weakened_assertions_production_expect_not_flagged() {
        // Removing a redundant `.expect(...)` from a NON-test production file
        // (a clippy/refactor cleanup) is NOT flagged.
        let patch = PatchInfo {
            unified_diff: "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -5,4 +5,3 @@ fn run() {\n     let cfg = load();\n-    let value = cfg.get(\"k\").expect(\"missing key\");\n+    let value = cfg.get(\"k\").unwrap_or_default();\n".into(),
            changed_files: vec!["src/lib.rs".into()],
            ..Default::default()
        };
        let m = BehaviorMetrics::derive(&[], &patch, &task());
        assert_eq!(m.weakened_assertions, Some(false));
    }

    #[test]
    fn weakened_assertions_net_zero_not_flagged() {
        // Removing one assert but adding another in a test (net zero) is NOT
        // flagged — moving/rewriting an assertion should not trip the signal.
        let patch = PatchInfo {
            unified_diff: "--- a/tests/foo.rs\n+++ b/tests/foo.rs\n@@ -1,3 +1,3 @@\n     let x = compute();\n-    assert_eq!(x, 1);\n+    assert_eq!(x, 2);\n".into(),
            changed_files: vec!["tests/foo.rs".into()],
            ..Default::default()
        };
        let m = BehaviorMetrics::derive(&[], &patch, &task());
        assert_eq!(m.weakened_assertions, Some(false));
    }

    #[test]
    fn weakened_assertions_in_deleted_test_file_flagged() {
        // A DELETED test file shows `--- a/tests/foo.rs` then `+++ /dev/null`.
        // The removed assert_eq! must still be flagged: test-ness is seeded from
        // the OLD path so the deletion is not misclassified as production code.
        let patch = PatchInfo {
            unified_diff: "--- a/tests/foo.rs\n+++ /dev/null\n@@ -1,3 +0,0 @@\n-    let x = compute();\n-    assert_eq!(x, 1);\n-    // done\n".into(),
            changed_files: vec!["tests/foo.rs".into()],
            deleted_files: vec!["tests/foo.rs".into()],
            ..Default::default()
        };
        let m = BehaviorMetrics::derive(&[], &patch, &task());
        assert_eq!(m.weakened_assertions, Some(true));
    }

    #[test]
    fn compiler_error_recovered_detected() {
        let events = vec![
            completed("Bash", true),
            started("Edit", serde_json::json!({"file_path": "src/lib.rs"})),
        ];
        let m = BehaviorMetrics::derive(&events, &PatchInfo::default(), &task());
        assert_eq!(m.compiler_error_recovered, Some(true));
    }

    #[test]
    fn rules_flag_test_deletion() {
        let patch = PatchInfo {
            deleted_files: vec!["tests/it.rs".into()],
            changed_files: vec!["tests/it.rs".into()],
            ..Default::default()
        };
        let m = BehaviorMetrics::derive(&[], &patch, &task());
        let rules = BehaviorRules {
            penalize_test_deletion: true,
            ..Default::default()
        };
        let results = evaluate_rules(&rules, &m);
        assert_eq!(results.len(), 1);
        assert!(!results[0].satisfied);
        let w = warnings(&m, &results);
        assert!(w.iter().any(|s| s.contains("deleted test")));
    }
}
