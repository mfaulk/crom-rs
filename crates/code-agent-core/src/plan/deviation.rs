//! Deviation classification for act-mode tool calls.
//!
//! Given an approved plan + a tool call, [`classify_tool_call`] returns
//! whether the call is in-scope, out-of-scope, ambiguous, or unrelated to
//! the plan (e.g. internal book-keeping tools the planning system itself
//! injects).

use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::OffsetDateTime;

use super::artifact::{
    PlanCommand, PlanDocument, PlanFileAccess, PlanFileRef, PlanId, PlanStep, WorkspaceRelativePath,
};
use super::options::PlanningOptions;
use super::tools::{is_safe_tool_pattern, PLAN_UPDATE_TOOL_NAME, SUBMIT_PLAN_TOOL_NAME};

/// What plan-scope evaluation decided about one tool call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PlanScopeDecision {
    InScope,
    OutOfScope { reason: String },
    Ambiguous { reason: String },
    NotApplicable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanDeviationSeverity {
    Low,
    Medium,
    High,
    External,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlanDeviation {
    pub plan_id: PlanId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_id: Option<String>,
    pub tool_name: String,
    #[serde(default)]
    pub requested_input: Value,
    pub reason: String,
    pub severity: PlanDeviationSeverity,
    #[serde(with = "time::serde::rfc3339")]
    pub occurred_at: OffsetDateTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanDeviationDecision {
    /// User approved this single deviation; downstream policy still applies.
    ApproveOnce,
    /// User wants the plan extended to cover this tool call before continuing.
    UpdatePlan,
    /// User wants to stop and trigger a re-plan flow.
    Replan,
    /// User denied this deviation; the act run should refuse the call.
    Deny,
}

/// Decide whether a tool call fits the approved plan's scope. The classifier
/// is intentionally conservative: anything mutating that doesn't match an
/// expected file/command becomes a deviation.
pub fn classify_tool_call(
    plan: &PlanDocument,
    options: &PlanningOptions,
    tool_name: &str,
    input: &Value,
) -> PlanScopeDecision {
    // Internal planning tools are always in-scope (PlanUpdate during act,
    // SubmitPlan during re-planning). Callers gate availability separately.
    if tool_name == PLAN_UPDATE_TOOL_NAME || tool_name == SUBMIT_PLAN_TOOL_NAME {
        return PlanScopeDecision::InScope;
    }

    // Read-side tools fall through to "in-scope" when exploratory reads are
    // permitted; otherwise they must match a planned file.
    let is_read_only = matches!(tool_name, "Read" | "Glob" | "Grep");

    // Tool-name allow check: the plan must mention the tool somewhere
    // (in `requested_tools` or any step's `expected_tools`). We do this
    // first so an obviously-approved tool name short-circuits the more
    // specific path/command checks below.
    let tool_in_plan = plan
        .requested_tools
        .iter()
        .any(|t| matches_tool_name(&t.tool_name, tool_name))
        || plan.steps.iter().any(|s| {
            s.expected_tools
                .iter()
                .any(|t| matches_tool_name(&t.tool_name, tool_name))
        });

    if !tool_in_plan {
        if is_read_only && options.exploratory_reads_during_act {
            return PlanScopeDecision::InScope;
        }
        return PlanScopeDecision::OutOfScope {
            reason: format!("tool `{tool_name}` is not listed in the approved plan"),
        };
    }

    // Now check tool-specific input constraints.
    match tool_name {
        "Bash" => classify_bash(plan, options, input),
        "Edit" | "Write" | "Read" => classify_file_tool(plan, options, tool_name, input),
        _ => {
            // Fallback: tool is mentioned in the plan but we don't have a
            // structured constraint to enforce. Permit, but mark Ambiguous
            // so downstream UI can flag it.
            PlanScopeDecision::Ambiguous {
                reason: format!("tool `{tool_name}` matched plan but no constraint to enforce"),
            }
        }
    }
}

fn matches_tool_name(planned: &str, actual: &str) -> bool {
    // Fail-closed: refuse to match unsafe patterns at the matcher itself,
    // not just at SubmitPlan parse time. A plan loaded from disk pre-
    // dating the parser-side validation (or smuggled in through some
    // other persistence path) can still carry patterns like `?*`,
    // `[a-z]*`, or `{foo,bar}*` that the underlying `globset` engine
    // would happily match against every tool. Returning `false` here
    // forces the user to re-submit a valid plan and is the same shape
    // check the parser applies — see [`is_safe_tool_pattern`].
    if !is_safe_tool_pattern(planned) {
        tracing::warn!(
            target: "plan.deviation",
            planned,
            "refusing to match unsafe planned tool_name pattern (fails shape check); \
             plan must be re-submitted with an exact name or `<literal_prefix>*`"
        );
        return false;
    }
    if planned == actual {
        return true;
    }
    // Glob support: `mcp__fake__*` etc. The shape check above already
    // guarantees `planned` is either an exact name (handled above) or
    // `<literal_prefix>*` with no other metacharacters, so the glob
    // compile here cannot expand to a broad wildcard.
    if planned.contains('*') {
        if let Ok(glob) = globset::Glob::new(planned) {
            return glob.compile_matcher().is_match(actual);
        }
    }
    false
}

/// Wrap a planned `command_regex` in `^…$` if it isn't already anchored,
/// so `cargo test` cannot match `&& cargo test` substrings within longer
/// shell commands. Public so the [`super::tools`] parser can also
/// validate the eventual anchored shape eagerly.
pub fn anchor_regex(re: &str) -> String {
    let trimmed = re.trim();
    let mut out = String::with_capacity(trimmed.len() + 2);
    if !trimmed.starts_with('^') {
        out.push('^');
    }
    out.push_str(trimmed);
    if !trimmed.ends_with('$') {
        out.push('$');
    }
    out
}

fn classify_bash(
    plan: &PlanDocument,
    options: &PlanningOptions,
    input: &Value,
) -> PlanScopeDecision {
    let cmd = input.get("command").and_then(Value::as_str).unwrap_or("");
    if cmd.is_empty() {
        return PlanScopeDecision::OutOfScope {
            reason: "Bash call has no `command` field".into(),
        };
    }

    // Collect every planned command (top-level + per step).
    let mut planned: Vec<&PlanCommand> = Vec::new();
    for s in &plan.steps {
        for c in &s.expected_commands {
            planned.push(c);
        }
    }

    let mut allow = false;
    for c in &planned {
        if c.command == cmd {
            allow = true;
            break;
        }
        if let Some(re) = c.command_regex.as_deref() {
            // Defense-in-depth: refuse to use a planned regex that
            // contains an inline flag group like `(?m)`. The SubmitPlan
            // parser already rejects these, but a plan loaded from
            // older stores or another path could still contain one,
            // and `(?m)` together with our `^…$` wrapping would re-
            // interpret the anchors as line anchors and authorize
            // newline-smuggled commands.
            if re.contains("(?") {
                continue;
            }
            // SAFETY: anchor the planned regex so a pattern like
            // `cargo test` does not silently authorize `echo hacked &&
            // cargo test`. We auto-prepend `^` and append `$` if the
            // plan author didn't already do so.
            let anchored = anchor_regex(re);
            if let Ok(rx) = regex::Regex::new(&anchored) {
                if rx.is_match(cmd) {
                    allow = true;
                    break;
                }
            }
        }
    }

    if allow {
        return PlanScopeDecision::InScope;
    }

    if options.strict_commands {
        return PlanScopeDecision::OutOfScope {
            reason: format!("Bash command `{cmd}` not listed in the approved plan"),
        };
    }
    PlanScopeDecision::Ambiguous {
        reason: format!("Bash command `{cmd}` not listed; strict_commands is off"),
    }
}

/// Map a tool name to the `PlanFileAccess` it implies, or `None` if the
/// tool is not a known file-touching tool. Returning `None` lets callers
/// fail safely (treat as Ambiguous) instead of panicking when a future
/// file tool is added without a matching mapping.
///
/// LINT: when adding a new file-touching tool, extend both this match
/// and the dispatch arm in [`classify_tool_call`] (`"Edit" | "Write" |
/// "Read"` branch). The two must stay in sync.
pub(crate) fn access_for_file_tool(tool_name: &str) -> Option<PlanFileAccess> {
    match tool_name {
        "Read" => Some(PlanFileAccess::Read),
        "Edit" => Some(PlanFileAccess::Edit),
        "Write" => Some(PlanFileAccess::Write),
        _ => None,
    }
}

fn classify_file_tool(
    plan: &PlanDocument,
    options: &PlanningOptions,
    tool_name: &str,
    input: &Value,
) -> PlanScopeDecision {
    let path_str = input.get("file_path").and_then(Value::as_str).unwrap_or("");
    if path_str.is_empty() {
        return PlanScopeDecision::OutOfScope {
            reason: format!("{tool_name} call has no `file_path`"),
        };
    }

    // LINT: keep this arm in sync with the dispatch in `classify_tool_call`
    // (`"Edit" | "Write" | "Read"` branch). When adding a new file-touching
    // tool here, also extend the dispatch arm and the corresponding
    // PlanFileAccess variant. Returning `None` for an unknown tool name
    // routes the call into the conservative Ambiguous fallthrough below
    // rather than panicking — but the contract is that every entry in the
    // dispatch arm has a matching entry here.
    let want = match access_for_file_tool(tool_name) {
        Some(a) => a,
        None => {
            return PlanScopeDecision::Ambiguous {
                reason: format!(
                    "file tool `{tool_name}` has no structured access mapping; \
                     classifier cannot enforce a constraint"
                ),
            };
        }
    };

    let mut all_files: Vec<&PlanFileRef> = Vec::new();
    for f in &plan.files {
        all_files.push(f);
    }
    for s in &plan.steps {
        for f in &s.expected_files {
            all_files.push(f);
        }
    }

    for f in &all_files {
        if file_paths_equal(&f.path, path_str) && file_access_satisfies(f.access, want) {
            return PlanScopeDecision::InScope;
        }
    }

    if want == PlanFileAccess::Read && options.exploratory_reads_during_act {
        return PlanScopeDecision::InScope;
    }

    PlanScopeDecision::OutOfScope {
        reason: format!(
            "{tool_name} on `{path_str}` ({}) not approved by the plan",
            access_label(want)
        ),
    }
}

fn file_paths_equal(planned: &WorkspaceRelativePath, actual: &str) -> bool {
    // Use Path::components to drop `./` segments and collapse `//` so
    // tool calls targeting `src/./parser.rs`, `src//parser.rs`, or
    // `./src/parser.rs` all compare equal to a planned `src/parser.rs`.
    // We deliberately stop short of resolving `..` — those should already
    // have been refused by `WorkspaceRelativePath::try_new` on the planned
    // side, and `..` in tool input is suspicious enough that string-level
    // failure is the right outcome.
    fn normalize(s: &str) -> String {
        use std::path::Component;
        let s = s.replace('\\', "/");
        let path = std::path::Path::new(&s);
        let mut parts: Vec<&std::ffi::OsStr> = Vec::new();
        for c in path.components() {
            match c {
                Component::Normal(n) => parts.push(n),
                Component::CurDir => {}
                // Anything else (RootDir, ParentDir, Prefix) — bail out
                // and return the raw string so the comparison fails
                // safely rather than silently rewriting unsafe segments.
                _ => return s,
            }
        }
        parts
            .into_iter()
            .filter_map(|p| p.to_str())
            .collect::<Vec<_>>()
            .join("/")
    }
    normalize(planned.as_str()) == normalize(actual)
}

fn file_access_satisfies(planned: PlanFileAccess, requested: PlanFileAccess) -> bool {
    use PlanFileAccess as A;
    match (planned, requested) {
        // Exact match always satisfies.
        (a, b) if a == b => true,
        // Edit covers Write/Create.
        (A::Edit, A::Write) | (A::Edit, A::Create) => true,
        // Write covers Create.
        (A::Write, A::Create) => true,
        // Read access alone never satisfies a mutating request.
        (A::Read, b) => b == A::Read,
        // Otherwise: the planned access doesn't cover the requested.
        _ => false,
    }
}

fn access_label(a: PlanFileAccess) -> &'static str {
    match a {
        PlanFileAccess::Read => "read",
        PlanFileAccess::Write => "write",
        PlanFileAccess::Edit => "edit",
        PlanFileAccess::Create => "create",
    }
}

/// Default severity for a deviation, derived from tool kind.
pub fn default_severity_for(tool_name: &str) -> PlanDeviationSeverity {
    if tool_name.starts_with("mcp__") {
        return PlanDeviationSeverity::External;
    }
    match tool_name {
        "Read" | "Glob" | "Grep" => PlanDeviationSeverity::Low,
        "Edit" | "Write" => PlanDeviationSeverity::High,
        "Bash" => PlanDeviationSeverity::High,
        "Agent" => PlanDeviationSeverity::Medium,
        _ => PlanDeviationSeverity::Medium,
    }
}

/// Walk a plan document and best-effort redact obvious secrets in every
/// field a user-supplied prompt could echo into. Plans should never carry
/// credentials, but the model frequently restates the user's request
/// verbatim into `summary`, `goal`, or step descriptions, so the
/// scrubber needs to cover free-form text too — not just the structured
/// metadata + tool-input fields the original implementation handled.
pub fn redact_secrets_in_plan(plan: &mut PlanDocument) {
    use crate::builtin_hooks::{redact_secrets_in_str, redact_secrets_in_value};

    if !plan.metadata.is_null() {
        plan.metadata = redact_secrets_in_value(&plan.metadata);
    }
    plan.user_request = redact_secrets_in_str(&plan.user_request);
    plan.summary = redact_secrets_in_str(&plan.summary);
    plan.goal = redact_secrets_in_str(&plan.goal);
    for s in plan.assumptions.iter_mut() {
        *s = redact_secrets_in_str(s);
    }
    for s in plan.constraints.iter_mut() {
        *s = redact_secrets_in_str(s);
    }
    for s in plan.success_criteria.iter_mut() {
        *s = redact_secrets_in_str(s);
    }
    if let Some(rb) = plan.rollback_plan.as_mut() {
        *rb = redact_secrets_in_str(rb);
    }
    for c in plan.context_gathered.iter_mut() {
        c.summary = redact_secrets_in_str(&c.summary);
    }
    for r in plan.risks.iter_mut() {
        r.description = redact_secrets_in_str(&r.description);
        if let Some(m) = r.mitigation.as_mut() {
            *m = redact_secrets_in_str(m);
        }
    }
    for v in plan.validations.iter_mut() {
        v.description = redact_secrets_in_str(&v.description);
        if let Some(c) = v.command.as_mut() {
            *c = redact_secrets_in_str(c);
        }
    }
    for t in &mut plan.requested_tools {
        if !t.input_constraints.is_null() {
            t.input_constraints = redact_secrets_in_value(&t.input_constraints);
        }
        if let Some(reason) = t.reason.as_mut() {
            *reason = redact_secrets_in_str(reason);
        }
    }
    for s in &mut plan.steps {
        s.title = redact_secrets_in_str(&s.title);
        s.description = redact_secrets_in_str(&s.description);
        if let Some(o) = s.expected_outcome.as_mut() {
            *o = redact_secrets_in_str(o);
        }
        for t in &mut s.expected_tools {
            if !t.input_constraints.is_null() {
                t.input_constraints = redact_secrets_in_value(&t.input_constraints);
            }
            if let Some(reason) = t.reason.as_mut() {
                *reason = redact_secrets_in_str(reason);
            }
        }
        for c in &mut s.expected_commands {
            c.command = redact_secrets_in_str(&c.command);
            if let Some(reason) = c.reason.as_mut() {
                *reason = redact_secrets_in_str(reason);
            }
        }
        if let Some(v) = s.validation.as_mut() {
            v.description = redact_secrets_in_str(&v.description);
            if let Some(c) = v.command.as_mut() {
                *c = redact_secrets_in_str(c);
            }
        }
    }
}

/// Convenience: locate the next pending step in execution order. Used by
/// the act driver to track progress.
pub fn next_pending_step(plan: &PlanDocument) -> Option<&PlanStep> {
    plan.steps
        .iter()
        .find(|s| matches!(s.status, super::artifact::PlanStepStatus::Pending))
}

/// Convenience: returns true iff every required step is completed.
pub fn all_steps_terminal(plan: &PlanDocument) -> bool {
    use super::artifact::PlanStepStatus as S;
    plan.steps
        .iter()
        .all(|s| matches!(s.status, S::Completed | S::Skipped | S::Failed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::PermissionKind;
    use crate::plan::artifact::{
        new_plan_id, PlanDocument, PlanFileAccess, PlanFileRef, PlanRiskLevel, PlanStep,
        PlanStepKind, PlanStepStatus, PlanToolUse, WorkspaceRelativePath,
    };
    use serde_json::json;

    fn plan_with_edit() -> PlanDocument {
        let mut p = PlanDocument::new_draft(new_plan_id(), "T", "u");
        p.files.push(PlanFileRef {
            path: WorkspaceRelativePath::try_new("src/parser.rs").unwrap(),
            access: PlanFileAccess::Edit,
            reason: None,
        });
        p.requested_tools.push(PlanToolUse {
            tool_name: "Edit".into(),
            permission_kind: PermissionKind::Mutating,
            input_constraints: serde_json::Value::Null,
            reason: None,
        });
        p.steps.push(PlanStep {
            id: "1".into(),
            title: "edit parser".into(),
            description: String::new(),
            status: PlanStepStatus::Pending,
            kind: PlanStepKind::Edit,
            expected_tools: vec![PlanToolUse {
                tool_name: "Edit".into(),
                permission_kind: PermissionKind::Mutating,
                input_constraints: serde_json::Value::Null,
                reason: None,
            }],
            expected_files: vec![PlanFileRef {
                path: WorkspaceRelativePath::try_new("src/parser.rs").unwrap(),
                access: PlanFileAccess::Edit,
                reason: None,
            }],
            expected_commands: Vec::new(),
            expected_outcome: None,
            risk: PlanRiskLevel::Low,
            requires_user_approval: false,
            validation: None,
        });
        p
    }

    #[test]
    // In-scope edit on the planned file is accepted.
    fn in_scope_edit() {
        let p = plan_with_edit();
        let opt = PlanningOptions::default();
        let d = classify_tool_call(&p, &opt, "Edit", &json!({"file_path": "src/parser.rs"}));
        assert_eq!(d, PlanScopeDecision::InScope);
    }

    #[test]
    // Out-of-scope edit on a different file is flagged.
    fn out_of_scope_edit() {
        let p = plan_with_edit();
        let opt = PlanningOptions::default();
        let d = classify_tool_call(&p, &opt, "Edit", &json!({"file_path": "src/lexer.rs"}));
        assert!(matches!(d, PlanScopeDecision::OutOfScope { .. }));
    }

    #[test]
    // Bash command must match plan via exact match or anchored regex.
    fn bash_regex_in_scope() {
        let mut p = plan_with_edit();
        // Anchored, optional-args regex: matches `cargo test`, `cargo check --all`,
        // but NOT trailing-injection forms like `cargo test && rm -rf /`.
        p.steps[0].expected_commands.push(PlanCommand {
            command: "cargo test".into(),
            command_regex: Some("^cargo (test|check)( .*)?".into()),
            cwd: None,
            reason: None,
        });
        p.requested_tools.push(PlanToolUse {
            tool_name: "Bash".into(),
            permission_kind: PermissionKind::Execution,
            input_constraints: serde_json::Value::Null,
            reason: None,
        });
        p.steps[0].expected_tools.push(PlanToolUse {
            tool_name: "Bash".into(),
            permission_kind: PermissionKind::Execution,
            input_constraints: serde_json::Value::Null,
            reason: None,
        });
        let opt = PlanningOptions::default();
        let d = classify_tool_call(&p, &opt, "Bash", &json!({"command": "cargo check --all"}));
        assert_eq!(d, PlanScopeDecision::InScope);
        let d2 = classify_tool_call(&p, &opt, "Bash", &json!({"command": "rm -rf /"}));
        assert!(matches!(d2, PlanScopeDecision::OutOfScope { .. }));
    }

    #[test]
    // A planned `cargo test` regex MUST NOT permit a tool call that
    // smuggles other commands in around it (`echo hacked && cargo test`,
    // `cargo test; rm -rf /`). The classifier auto-anchors the regex
    // so substring matches don't constitute scope approval.
    fn bash_regex_does_not_allow_command_injection() {
        let mut p = plan_with_edit();
        p.steps[0].expected_commands.push(PlanCommand {
            command: "cargo test".into(),
            command_regex: Some("cargo test".into()), // unanchored on purpose
            cwd: None,
            reason: None,
        });
        p.requested_tools.push(PlanToolUse {
            tool_name: "Bash".into(),
            permission_kind: PermissionKind::Execution,
            input_constraints: serde_json::Value::Null,
            reason: None,
        });
        p.steps[0].expected_tools.push(PlanToolUse {
            tool_name: "Bash".into(),
            permission_kind: PermissionKind::Execution,
            input_constraints: serde_json::Value::Null,
            reason: None,
        });
        let opt = PlanningOptions::default();
        // Exact match — still allowed.
        assert_eq!(
            classify_tool_call(&p, &opt, "Bash", &json!({"command": "cargo test"})),
            PlanScopeDecision::InScope
        );
        // Trailing injection — denied.
        let evil = classify_tool_call(
            &p,
            &opt,
            "Bash",
            &json!({"command": "echo hacked && cargo test"}),
        );
        assert!(matches!(evil, PlanScopeDecision::OutOfScope { .. }));
        let evil2 = classify_tool_call(
            &p,
            &opt,
            "Bash",
            &json!({"command": "cargo test; rm -rf /"}),
        );
        assert!(matches!(evil2, PlanScopeDecision::OutOfScope { .. }));
    }

    #[test]
    // Pins the underlying bypass: an inline `(?m)` flag turns the
    // auto-anchored `^…$` wrapping into line anchors, so a plain
    // regex::Regex would match a newline-smuggled command. This test
    // is the upstream proof of the bug, alongside the SubmitPlan-parser
    // rejection test in `plan::tools`.
    fn anchored_multiline_flag_matches_newline_smuggled_command() {
        let bypass = regex::Regex::new("^(?m)^cargo test$$").unwrap();
        assert!(bypass.is_match("cargo test\nrm -rf ~/"));
    }

    #[test]
    // Defense-in-depth: even if a plan somehow carries a `(?m)`
    // regex (e.g. loaded from disk pre-fix), `classify_bash` must
    // refuse to authorize via that regex. The exact `cargo test`
    // string is still allowed via the structural `command` field.
    fn classify_bash_skips_flag_group_regex() {
        let mut p = plan_with_edit();
        p.steps[0].expected_commands.push(PlanCommand {
            command: "cargo test".into(),
            command_regex: Some("(?m)^cargo test$".into()),
            cwd: None,
            reason: None,
        });
        p.requested_tools.push(PlanToolUse {
            tool_name: "Bash".into(),
            permission_kind: PermissionKind::Execution,
            input_constraints: serde_json::Value::Null,
            reason: None,
        });
        p.steps[0].expected_tools.push(PlanToolUse {
            tool_name: "Bash".into(),
            permission_kind: PermissionKind::Execution,
            input_constraints: serde_json::Value::Null,
            reason: None,
        });
        let opt = PlanningOptions::default();
        // Exact `cargo test` still matches the literal `command` field.
        assert_eq!(
            classify_tool_call(&p, &opt, "Bash", &json!({"command": "cargo test"})),
            PlanScopeDecision::InScope
        );
        // Newline-smuggled form is NOT authorized: the regex is
        // skipped, and the literal command does not match.
        let smuggled = classify_tool_call(
            &p,
            &opt,
            "Bash",
            &json!({"command": "cargo test\nrm -rf ~/"}),
        );
        assert!(
            !matches!(smuggled, PlanScopeDecision::InScope),
            "newline-smuggled command must not be in-scope, got {smuggled:?}"
        );
    }

    #[test]
    // file_paths_equal collapses `./` and `//` so semantically-equivalent
    // forms compare equal to the planned path.
    fn path_equality_handles_dot_and_double_slash() {
        let p = plan_with_edit();
        let opt = PlanningOptions::default();
        for actual in &[
            "src/parser.rs",
            "./src/parser.rs",
            "src/./parser.rs",
            "src//parser.rs",
        ] {
            assert_eq!(
                classify_tool_call(&p, &opt, "Edit", &json!({"file_path": actual})),
                PlanScopeDecision::InScope,
                "actual={actual} should be in scope"
            );
        }
    }

    #[test]
    // Free-form plan body fields are scrubbed for well-known secret
    // shapes before persistence. Defends against the model echoing the
    // user's prompt (which may contain pasted credentials) into
    // summary/goal/etc.
    fn redact_secrets_in_plan_scrubs_body_strings() {
        let mut p = plan_with_edit();
        p.user_request = "Use sk-ant-api03-aaaaaaaaaaaaaaaaaaaa to call".into();
        p.summary = "Bearer abcdefghijABCDEF1234".into();
        p.goal = "Use AKIAABCDEFGHIJKLMNOP for AWS".into();
        p.assumptions.push("Token: ghp_abcdefghijklmnopqrst".into());
        p.success_criteria.push("Plain text, no secret".into());
        super::redact_secrets_in_plan(&mut p);
        assert!(!p.user_request.contains("sk-ant-api03"));
        assert!(p.summary.contains("[REDACTED]"));
        assert!(p.goal.contains("[REDACTED]"));
        assert!(p.assumptions[0].contains("[REDACTED]"));
        assert!(p.success_criteria[0].contains("Plain text"));
    }

    #[test]
    // Exploratory reads in act mode are accepted when the option is on.
    fn exploratory_reads_default_on() {
        let p = plan_with_edit();
        let opt = PlanningOptions::default();
        let d = classify_tool_call(&p, &opt, "Read", &json!({"file_path": "README.md"}));
        assert_eq!(d, PlanScopeDecision::InScope);
    }

    #[test]
    // PlanUpdate is always in-scope so the act loop can drive its own progress.
    fn plan_update_always_in_scope() {
        let p = plan_with_edit();
        let opt = PlanningOptions::default();
        let d = classify_tool_call(&p, &opt, PLAN_UPDATE_TOOL_NAME, &json!({}));
        assert_eq!(d, PlanScopeDecision::InScope);
    }

    #[test]
    // matches_tool_name returns false for `?*` even though globset would
    // happily compile and match it against every real tool name. Closes
    // the legacy-plan bypass for plans persisted before parser-side
    // validation existed.
    fn matches_tool_name_rejects_bare_question_star() {
        assert!(!super::matches_tool_name("?*", "Bash"));
        assert!(!super::matches_tool_name("?*", "Read"));
        assert!(!super::matches_tool_name("?*", "mcp__github__create_issue"));
    }

    #[test]
    // matches_tool_name returns false for a character-class wildcard
    // `[a-z]*`; the shape check refuses any glob metacharacter outside
    // a single trailing `*`.
    fn matches_tool_name_rejects_character_class_wildcard() {
        assert!(!super::matches_tool_name("[a-z]*", "Bash"));
        assert!(!super::matches_tool_name("[a-z]*", "read"));
    }

    #[test]
    // matches_tool_name returns false for a brace-alternation pattern
    // `{foo,bar}*`; globset would expand it, but the shape check
    // forbids `{`/`}` entirely.
    fn matches_tool_name_rejects_brace_alternation() {
        assert!(!super::matches_tool_name("{foo,bar}*", "foo"));
        assert!(!super::matches_tool_name("{foo,bar}*", "bar_tool"));
    }

    #[test]
    // matches_tool_name returns false for a lone `?` because the only
    // permitted glob shape is `<non-empty-literal-prefix>*`.
    fn matches_tool_name_rejects_lone_question_mark() {
        assert!(!super::matches_tool_name("?", "B"));
        assert!(!super::matches_tool_name("?", "Bash"));
    }

    #[test]
    // matches_tool_name still accepts the canonical MCP family glob
    // `mcp__github__*` against a concrete tool in that family — the
    // tightening must not regress the legitimate use case.
    fn matches_tool_name_accepts_mcp_prefix_glob() {
        assert!(super::matches_tool_name(
            "mcp__github__*",
            "mcp__github__create_issue"
        ));
        assert!(super::matches_tool_name(
            "mcp__github__*",
            "mcp__github__list"
        ));
        assert!(!super::matches_tool_name(
            "mcp__github__*",
            "mcp__gitlab__create_issue"
        ));
    }

    #[test]
    // matches_tool_name still accepts an exact tool name against itself
    // (no glob involvement). Pins the exact-match path through the
    // shape check.
    fn matches_tool_name_accepts_exact_name() {
        assert!(super::matches_tool_name("Bash", "Bash"));
        assert!(!super::matches_tool_name("Bash", "Edit"));
    }

    #[test]
    // access_for_file_tool returns None for an unknown tool name (no
    // panic). Maintenance contract: when a new file tool is added to the
    // dispatch arm in `classify_tool_call`, this map must learn it too.
    fn access_for_file_tool_returns_none_for_unknown_name() {
        assert_eq!(
            super::access_for_file_tool("Read"),
            Some(PlanFileAccess::Read)
        );
        assert_eq!(
            super::access_for_file_tool("Edit"),
            Some(PlanFileAccess::Edit)
        );
        assert_eq!(
            super::access_for_file_tool("Write"),
            Some(PlanFileAccess::Write)
        );
        // Unknown future tool: must NOT panic and must return None so
        // callers can treat it as Ambiguous.
        assert_eq!(super::access_for_file_tool("FutureDelete"), None);
        assert_eq!(super::access_for_file_tool("MoveFile"), None);
        assert_eq!(super::access_for_file_tool(""), None);
    }

    #[test]
    // classify_file_tool routes an unknown file-tool name through the
    // Ambiguous fallback instead of panicking. Pins #81: the prior
    // `unreachable!()` in the match would have crashed if the dispatch
    // arm in classify_tool_call ever grew without updating this match.
    fn classify_file_tool_unknown_tool_is_ambiguous_not_panic() {
        let p = plan_with_edit();
        let opt = PlanningOptions::default();
        // Call the helper directly so we exercise the previously-
        // unreachable arm without having to extend `classify_tool_call`'s
        // dispatch in the test.
        let d = super::classify_file_tool(
            &p,
            &opt,
            "FutureDelete",
            &json!({"file_path": "src/parser.rs"}),
        );
        match d {
            PlanScopeDecision::Ambiguous { reason } => {
                assert!(
                    reason.contains("FutureDelete"),
                    "ambiguous reason should name the offending tool, got: {reason}"
                );
            }
            other => panic!("expected Ambiguous for unknown file tool, got: {other:?}"),
        }
    }
}
