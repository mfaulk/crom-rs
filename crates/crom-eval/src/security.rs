//! Security audit export for eval runs (M17.3): `security.jsonl` holds every
//! `SecurityDecision` record and `security-summary.json` the audit-coverage
//! metric (share of executed tool calls that have a decision record).

use std::collections::{BTreeMap, BTreeSet};

use crom_core::events::{SecurityDecision, SecurityOutcome};
use crom_sdk::SdkEvent;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SecurityAuditSummary {
    /// Number of `SecurityDecision` records.
    pub decisions: usize,
    /// Tool calls the agent attempted (`ToolCallStarted`).
    pub attempted: usize,
    pub blocked: usize,
    pub executed: usize,
    pub failed: usize,
    /// Blocked records per enforcement layer (`policy`, `hook`, ...).
    pub blocked_by_layer: BTreeMap<String, usize>,
    /// Tool executions (`ToolCallCompleted`).
    pub tool_executions: usize,
    /// Tool executions with a matching decision record (same `tool_use_id`).
    pub executions_with_decision: usize,
    /// `executions_with_decision / tool_executions`; `None` when nothing ran.
    pub audit_coverage: Option<f64>,
}

/// Every `SecurityDecision` in `events`, in emission order.
pub fn security_records(events: &[SdkEvent]) -> Vec<&SecurityDecision> {
    events
        .iter()
        .filter_map(|e| match e {
            SdkEvent::SecurityDecision(d) => Some(d),
            _ => None,
        })
        .collect()
}

pub fn summarize(events: &[SdkEvent]) -> SecurityAuditSummary {
    let records = security_records(events);
    let mut s = SecurityAuditSummary {
        decisions: records.len(),
        ..Default::default()
    };
    let mut decided: BTreeSet<&str> = BTreeSet::new();
    for d in &records {
        decided.insert(d.tool_use_id.as_str());
        match d.outcome {
            SecurityOutcome::Blocked => {
                s.blocked += 1;
                let layer = serde_json::to_value(d.enforcement)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default();
                *s.blocked_by_layer.entry(layer).or_default() += 1;
            }
            SecurityOutcome::Executed => s.executed += 1,
            SecurityOutcome::Failed => s.failed += 1,
        }
    }
    for e in events {
        match e {
            SdkEvent::ToolCallStarted(_) => s.attempted += 1,
            SdkEvent::ToolCallCompleted(c) => {
                s.tool_executions += 1;
                if decided.contains(c.tool_use_id.as_str()) {
                    s.executions_with_decision += 1;
                }
            }
            _ => {}
        }
    }
    s.audit_coverage = (s.tool_executions > 0)
        .then(|| s.executions_with_decision as f64 / s.tool_executions as f64);
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crom_core::events::{EnforcementLayer, SecurityPrincipal};
    use crom_core::PolicyOutcome;

    fn decision(id: &str, outcome: SecurityOutcome, layer: EnforcementLayer) -> SdkEvent {
        SdkEvent::SecurityDecision(SecurityDecision {
            session_id: crom_core::SessionId::new(),
            run_id: crom_core::RunId::new(),
            turn_index: 1,
            principal: SecurityPrincipal::Session,
            tool_use_id: id.into(),
            tool_name: "Bash".into(),
            action_class: None,
            resource: None,
            policy_source: None,
            rule: None,
            decision: PolicyOutcome::Allow,
            enforcement: layer,
            outcome,
            reason: None,
            guard_decision_id: None,
            guard_approval: None,
        })
    }

    fn completed(id: &str) -> SdkEvent {
        SdkEvent::ToolCallCompleted(crom_sdk::event::ToolCallCompletedEvent {
            run_id: crom_sdk::RunId::new(),
            tool_name: "Bash".into(),
            tool_use_id: id.into(),
            is_error: false,
            duration_ms: 1,
        })
    }

    #[test]
    // Coverage is the share of completed tool calls that have a decision record, with blocks counted per layer.
    fn summary_computes_coverage_and_block_counts() {
        let events = vec![
            decision("a", SecurityOutcome::Executed, EnforcementLayer::Policy),
            completed("a"),
            completed("b"),
            decision("c", SecurityOutcome::Blocked, EnforcementLayer::Hook),
        ];
        let s = summarize(&events);
        assert_eq!(s.decisions, 2);
        assert_eq!(s.executed, 1);
        assert_eq!(s.blocked, 1);
        assert_eq!(s.blocked_by_layer.get("hook"), Some(&1));
        assert_eq!(s.tool_executions, 2);
        assert_eq!(s.executions_with_decision, 1);
        assert_eq!(s.audit_coverage, Some(0.5));
    }

    #[test]
    // With no tool executions the coverage metric is undefined rather than zero.
    fn summary_coverage_is_none_without_executions() {
        assert_eq!(summarize(&[]).audit_coverage, None);
    }
}
