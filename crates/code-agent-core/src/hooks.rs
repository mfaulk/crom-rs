//! Hook system. Hooks observe and influence the agent loop at well-defined
//! events: user prompt submission, before/after tool execution, batch end,
//! and stop. Multiple hooks can register against the same event; their
//! outputs are merged with the priority `Deny > Ask > Allow > AddContext >
//! Continue`.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use globset::{Glob, GlobMatcher};
use regex::Regex;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::options::SessionId;
use crate::run_id::RunId;
use crate::tool::ToolResult;

/// Hook events fired by the agent loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "snake_case")]
pub enum HookEventName {
    UserPromptSubmit,
    PreToolUse,
    PostToolUse,
    PostToolUseFailure,
    PostToolBatch,
    Stop,
}

impl HookEventName {
    pub fn as_str(&self) -> &'static str {
        match self {
            HookEventName::UserPromptSubmit => "UserPromptSubmit",
            HookEventName::PreToolUse => "PreToolUse",
            HookEventName::PostToolUse => "PostToolUse",
            HookEventName::PostToolUseFailure => "PostToolUseFailure",
            HookEventName::PostToolBatch => "PostToolBatch",
            HookEventName::Stop => "Stop",
        }
    }
}

/// Reason a `Stop` hook fired. Hooks can branch on this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    Success,
    MaxTurns,
    Cancelled,
    Error,
}

/// Input passed to a hook.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookInput {
    pub event: HookEventName,
    pub session_id: SessionId,
    pub cwd: PathBuf,
    pub run_id: RunId,
    pub turn_index: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_input: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_result: Option<ToolResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<StopReason>,
    #[serde(skip_serializing_if = "serde_json::Value::is_null")]
    pub metadata: serde_json::Value,
}

impl HookInput {
    pub fn new(event: HookEventName, session_id: SessionId, cwd: PathBuf, run_id: RunId) -> Self {
        Self {
            event,
            session_id,
            cwd,
            run_id,
            turn_index: 0,
            tool_name: None,
            tool_input: None,
            tool_result: None,
            prompt: None,
            stop_reason: None,
            metadata: serde_json::Value::Null,
        }
    }
}

/// Output a hook can return.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HookOutput {
    /// No opinion; let other hooks/policy decide.
    Continue,
    /// Allow this call. Optionally rewrite the tool input.
    Allow {
        #[serde(skip_serializing_if = "Option::is_none")]
        updated_input: Option<serde_json::Value>,
    },
    /// Deny this call.
    Deny { reason: String },
    /// Ask the user.
    Ask { reason: String },
    /// Inject extra context (only meaningful for `UserPromptSubmit`).
    AddContext { content: String },
}

#[derive(Debug, Error)]
pub enum HookError {
    /// The tool-name pattern supplied to a hook registration method is invalid.
    #[error("invalid hook pattern {pattern:?} for event {event:?}: {parser_error}")]
    Pattern {
        /// The hook event the pattern was registered for.
        event: HookEventName,
        /// The pattern string the caller supplied.
        pattern: String,
        /// Underlying parser error from `ToolMatcher::parse`.
        parser_error: String,
    },
    #[error("{0}")]
    Other(String),
}

#[async_trait]
pub trait Hook: Send + Sync {
    async fn call(&self, input: HookInput) -> Result<HookOutput, HookError>;
}

/// Compiled regex/glob matcher for tool names. None matches all tools.
#[derive(Clone)]
pub enum ToolMatcher {
    All,
    Glob(GlobMatcher),
    Regex(Regex),
}

impl std::fmt::Debug for ToolMatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ToolMatcher::All => f.write_str("ToolMatcher::All"),
            ToolMatcher::Glob(_) => f.write_str("ToolMatcher::Glob"),
            ToolMatcher::Regex(r) => f
                .debug_tuple("ToolMatcher::Regex")
                .field(&r.as_str())
                .finish(),
        }
    }
}

impl ToolMatcher {
    /// Build a matcher from a `pattern` string. The pattern may contain `|`
    /// to indicate alternation (e.g. `Edit|Write`), `*` for glob, or be a
    /// literal name. The string `*` matches all tools.
    pub fn parse(pattern: &str) -> Result<Self, String> {
        if pattern == "*" {
            return Ok(ToolMatcher::All);
        }
        if pattern.contains('|') {
            // Treat as anchored regex alternation.
            let escaped: Vec<String> = pattern
                .split('|')
                .map(|p| regex::escape(p.trim()))
                .collect();
            let combined = format!("^({})$", escaped.join("|"));
            let re = Regex::new(&combined).map_err(|e| e.to_string())?;
            return Ok(ToolMatcher::Regex(re));
        }
        if pattern.contains('*') || pattern.contains('?') {
            let g = Glob::new(pattern).map_err(|e| e.to_string())?;
            return Ok(ToolMatcher::Glob(g.compile_matcher()));
        }
        // Literal exact match.
        let re = Regex::new(&format!("^{}$", regex::escape(pattern))).map_err(|e| e.to_string())?;
        Ok(ToolMatcher::Regex(re))
    }

    pub fn matches(&self, tool_name: &str) -> bool {
        match self {
            ToolMatcher::All => true,
            ToolMatcher::Glob(g) => g.is_match(tool_name),
            ToolMatcher::Regex(r) => r.is_match(tool_name),
        }
    }
}

/// One registered hook with its matcher.
#[derive(Clone)]
pub struct HookMatcher {
    pub event: HookEventName,
    pub matcher: ToolMatcher,
    pub hook: Arc<dyn Hook>,
}

impl std::fmt::Debug for HookMatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HookMatcher")
            .field("event", &self.event)
            .field("matcher", &self.matcher)
            .finish_non_exhaustive()
    }
}

/// Registered hooks for an `AgentClient`.
#[derive(Clone, Debug, Default)]
pub struct Hooks {
    matchers: Vec<HookMatcher>,
}

impl Hooks {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a hook for a given event with a tool-name matcher pattern.
    ///
    /// Returns `Err(HookError::Pattern)` if `pattern` fails to compile, so
    /// callers learn about typos at registration time instead of silently
    /// falling back to "match every tool".
    pub fn add(
        mut self,
        event: HookEventName,
        pattern: &str,
        hook: Arc<dyn Hook>,
    ) -> Result<Self, HookError> {
        let matcher = ToolMatcher::parse(pattern).map_err(|e| HookError::Pattern {
            event,
            pattern: pattern.to_owned(),
            parser_error: e,
        })?;
        self.matchers.push(HookMatcher {
            event,
            matcher,
            hook,
        });
        Ok(self)
    }

    /// Register a hook that fires on every `UserPromptSubmit` event.
    ///
    /// Infallible because the pattern is always `"*"`.
    pub fn user_prompt_submit(self, hook: Arc<dyn Hook>) -> Self {
        self.add(HookEventName::UserPromptSubmit, "*", hook)
            .expect("\"*\" is always a valid pattern")
    }

    pub fn pre_tool_use(self, pattern: &str, hook: Arc<dyn Hook>) -> Result<Self, HookError> {
        self.add(HookEventName::PreToolUse, pattern, hook)
    }

    pub fn post_tool_use(self, pattern: &str, hook: Arc<dyn Hook>) -> Result<Self, HookError> {
        self.add(HookEventName::PostToolUse, pattern, hook)
    }

    pub fn post_tool_use_failure(
        self,
        pattern: &str,
        hook: Arc<dyn Hook>,
    ) -> Result<Self, HookError> {
        self.add(HookEventName::PostToolUseFailure, pattern, hook)
    }

    /// Register a hook that fires after every tool batch.
    ///
    /// Infallible because the pattern is always `"*"`.
    pub fn post_tool_batch(self, hook: Arc<dyn Hook>) -> Self {
        self.add(HookEventName::PostToolBatch, "*", hook)
            .expect("\"*\" is always a valid pattern")
    }

    /// Register a hook that fires on agent stop.
    ///
    /// Infallible because the pattern is always `"*"`.
    pub fn stop(self, hook: Arc<dyn Hook>) -> Self {
        self.add(HookEventName::Stop, "*", hook)
            .expect("\"*\" is always a valid pattern")
    }

    pub fn matchers(&self) -> &[HookMatcher] {
        &self.matchers
    }

    pub fn is_empty(&self) -> bool {
        self.matchers.is_empty()
    }

    /// Run all hooks for `input.event` matching `tool_name` (when relevant) in
    /// registration order. Returns the merged outputs and individual outputs
    /// for telemetry.
    pub async fn dispatch(&self, input: &HookInput) -> Vec<HookCallRecord> {
        let mut records = Vec::new();
        for m in &self.matchers {
            if m.event != input.event {
                continue;
            }
            let tool_match = match input.tool_name.as_deref() {
                Some(n) => m.matcher.matches(n),
                None => matches!(m.matcher, ToolMatcher::All),
            };
            if !tool_match {
                continue;
            }
            let result = m.hook.call(input.clone()).await;
            records.push(HookCallRecord {
                event: m.event,
                tool_name: input.tool_name.clone(),
                output: result,
            });
        }
        records
    }
}

/// One hook invocation result (used for logging and decision merging).
#[derive(Debug)]
pub struct HookCallRecord {
    pub event: HookEventName,
    pub tool_name: Option<String>,
    pub output: Result<HookOutput, HookError>,
}

/// Aggregated decision from a list of hook calls.
#[derive(Debug, Clone, PartialEq)]
pub enum HookAggregateDecision {
    Continue,
    Allow {
        updated_input: Option<serde_json::Value>,
    },
    Deny {
        reason: String,
    },
    Ask {
        reason: String,
    },
}

/// Concatenated `AddContext` outputs (preserves registration order). Empty
/// when no hook returned `AddContext`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HookAddedContext(pub String);

/// Merge a list of hook outputs by priority.
///
/// Priority: `Deny` > `Ask` > `Allow` > `AddContext` > `Continue`.
pub fn aggregate(records: &[HookCallRecord]) -> (HookAggregateDecision, HookAddedContext) {
    let mut decision: HookAggregateDecision = HookAggregateDecision::Continue;
    let mut updated_input: Option<serde_json::Value> = None;
    let mut added = String::new();

    for rec in records {
        match &rec.output {
            Ok(HookOutput::Deny { reason }) => {
                return (
                    HookAggregateDecision::Deny {
                        reason: reason.clone(),
                    },
                    HookAddedContext(added),
                );
            }
            Ok(HookOutput::Ask { reason }) => {
                if !matches!(decision, HookAggregateDecision::Deny { .. }) {
                    decision = HookAggregateDecision::Ask {
                        reason: reason.clone(),
                    };
                }
            }
            Ok(HookOutput::Allow { updated_input: ui }) => {
                if matches!(decision, HookAggregateDecision::Continue) {
                    decision = HookAggregateDecision::Allow {
                        updated_input: ui.clone(),
                    };
                }
                if let Some(v) = ui {
                    updated_input = Some(v.clone());
                }
            }
            Ok(HookOutput::AddContext { content }) => {
                if !added.is_empty() {
                    added.push_str("\n\n");
                }
                added.push_str(content);
            }
            Ok(HookOutput::Continue) | Err(_) => {}
        }
    }

    // If we ended in Allow, propagate any updated_input that came through.
    if let HookAggregateDecision::Allow { .. } = decision {
        decision = HookAggregateDecision::Allow { updated_input };
    }

    (decision, HookAddedContext(added))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FixedHook(HookOutput);

    #[async_trait]
    impl Hook for FixedHook {
        async fn call(&self, _input: HookInput) -> Result<HookOutput, HookError> {
            Ok(self.0.clone())
        }
    }

    fn rec(out: HookOutput) -> HookCallRecord {
        HookCallRecord {
            event: HookEventName::PreToolUse,
            tool_name: Some("X".into()),
            output: Ok(out),
        }
    }

    #[test]
    // Aggregation rule: Deny wins over every other decision.
    fn deny_wins() {
        let records = vec![
            rec(HookOutput::Allow {
                updated_input: None,
            }),
            rec(HookOutput::Deny {
                reason: "no".into(),
            }),
            rec(HookOutput::Allow {
                updated_input: None,
            }),
        ];
        let (d, _) = aggregate(&records);
        assert!(matches!(d, HookAggregateDecision::Deny { .. }));
    }

    #[test]
    // Aggregation rule: Ask outranks Allow but yields to Deny.
    fn ask_outranks_allow() {
        let records = vec![
            rec(HookOutput::Allow {
                updated_input: None,
            }),
            rec(HookOutput::Ask { reason: "?".into() }),
        ];
        let (d, _) = aggregate(&records);
        assert!(matches!(d, HookAggregateDecision::Ask { .. }));
    }

    #[test]
    // Aggregation rule: with only AddContext, decision stays Continue but
    // the added context is concatenated in order.
    fn add_context_concatenates() {
        let records = vec![
            rec(HookOutput::AddContext {
                content: "one".into(),
            }),
            rec(HookOutput::AddContext {
                content: "two".into(),
            }),
        ];
        let (d, ctx) = aggregate(&records);
        assert!(matches!(d, HookAggregateDecision::Continue));
        assert_eq!(ctx.0, "one\n\ntwo");
    }

    #[test]
    // Allow with updated_input is preserved through aggregation.
    fn allow_passes_updated_input() {
        let records = vec![rec(HookOutput::Allow {
            updated_input: Some(serde_json::json!({"x": 1})),
        })];
        let (d, _) = aggregate(&records);
        match d {
            HookAggregateDecision::Allow { updated_input } => {
                assert_eq!(updated_input.unwrap()["x"], 1);
            }
            _ => panic!("expected Allow"),
        }
    }

    #[tokio::test]
    // Hooks dispatch only fires hooks whose event and matcher match.
    async fn dispatch_filters_by_event_and_matcher() {
        let hooks = Hooks::new()
            .pre_tool_use("Bash", Arc::new(FixedHook(HookOutput::Continue)))
            .unwrap()
            .post_tool_use("*", Arc::new(FixedHook(HookOutput::Continue)))
            .unwrap();
        let input = HookInput {
            tool_name: Some("Bash".into()),
            ..HookInput::new(
                HookEventName::PreToolUse,
                SessionId::from("s".to_string()),
                PathBuf::from("/"),
                RunId::from("r".to_string()),
            )
        };
        let records = hooks.dispatch(&input).await;
        assert_eq!(records.len(), 1);
    }

    #[test]
    // An invalid glob pattern (unbalanced bracket + wildcard) returns
    // Err(HookError::Pattern) carrying the event, the bad pattern, and the
    // underlying parser error — instead of silently matching all tools.
    fn add_with_bad_pattern_returns_err() {
        let h = Arc::new(FixedHook(HookOutput::Continue));
        // "[bad*" triggers the glob path (contains '*') with an unbalanced '['.
        let result = Hooks::new().pre_tool_use("[bad*", h);
        let err = result.expect_err("expected Err(HookError::Pattern)");
        match &err {
            HookError::Pattern {
                event,
                pattern,
                parser_error,
            } => {
                assert_eq!(*event, HookEventName::PreToolUse);
                assert_eq!(pattern, "[bad*");
                assert!(
                    !parser_error.is_empty(),
                    "underlying parser error must be preserved"
                );
            }
            other => panic!("expected HookError::Pattern, got {other:?}"),
        }
        // The Display form must surface the event and pattern so operators
        // can locate the bad registration.
        let msg = format!("{err}");
        assert!(msg.contains("[bad*"));
        assert!(msg.contains("PreToolUse"));
    }

    #[test]
    // A bad pattern in a chained call propagates the error (carrying the
    // event of the failing call) and discards the builder.
    fn chained_bad_pattern_propagates_err() {
        let h1 = Arc::new(FixedHook(HookOutput::Continue));
        let h2 = Arc::new(FixedHook(HookOutput::Continue));
        let result = Hooks::new()
            .pre_tool_use("Bash", h1)
            .and_then(|hooks| hooks.post_tool_use("[bad*", h2));
        let err = result.expect_err("expected Err(HookError::Pattern)");
        let HookError::Pattern { event, pattern, .. } = &err else {
            panic!("expected HookError::Pattern, got {err:?}");
        };
        assert_eq!(*event, HookEventName::PostToolUse);
        assert_eq!(pattern, "[bad*");
    }

    #[test]
    // ToolMatcher parses simple alternation patterns.
    fn matcher_parses_alternation() {
        let m = ToolMatcher::parse("Edit|Write").unwrap();
        assert!(m.matches("Edit"));
        assert!(m.matches("Write"));
        assert!(!m.matches("Bash"));
    }

    #[test]
    // ToolMatcher `*` matches everything.
    fn matcher_star_matches_all() {
        let m = ToolMatcher::parse("*").unwrap();
        assert!(m.matches("Anything"));
        assert!(m.matches(""));
    }
}
