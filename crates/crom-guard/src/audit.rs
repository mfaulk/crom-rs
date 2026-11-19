use serde::{Deserialize, Serialize};

use crate::decision::DenyReason;
use crate::ids::{
    ActionId, AgentId, AppId, ApprovalId, DecisionId, GrantId, PolicyId, RunId, TenantId, UserId,
};
use crate::time::Timestamp;
use crate::value::EntityUid;

/// Audit format identifier for records produced by this crate.
pub const AUDIT_FORMAT: &str = "guard-audit/v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordKind {
    /// Written before execution; if it cannot be written nothing executes.
    Decision,
    /// Written after execution.
    Outcome,
}

/// Which stage decided (MILESTONE_18 §12.1 `enforcement`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Enforcement {
    GuardSession,
    GuardValidation,
    GuardPolicy,
    GuardChain,
    GuardApproval,
    ExecutorPrecondition,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionKind {
    Allow,
    Deny,
    ApprovalRequired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Blocked,
    Executed,
    Failed,
    RevokedDuringExecution,
}

/// Identity fields, copied from the grant record (trusted). Absent when
/// the session could not be authenticated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditIdentity {
    pub issuer: AppId,
    pub tenant: TenantId,
    pub delegator: UserId,
    pub agent: AgentId,
    pub run_id: RunId,
    pub grant_id: GrantId,
    pub grant_chain: Vec<GrantId>,
}

impl From<&crate::TrustedPrincipal> for AuditIdentity {
    fn from(p: &crate::TrustedPrincipal) -> Self {
        Self {
            issuer: p.issuer().clone(),
            tenant: p.tenant().clone(),
            delegator: p.delegator().id().clone(),
            agent: p.agent().clone(),
            run_id: p.run_id().clone(),
            grant_id: p.grant_id().clone(),
            grant_chain: p.grant_chain().to_vec(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Durations {
    pub policy_us: Option<u64>,
    pub resolve_us: Option<u64>,
    pub execute_us: Option<u64>,
}

/// Correlation values as the agent supplied them. Kept apart from trusted
/// fields and never used for decisions.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UntrustedCorrelation {
    pub tool_use_id: Option<String>,
    pub client_request_id: Option<String>,
    /// The requested tool name, recorded when it maps to no action.
    pub requested_tool: Option<String>,
    pub client_info: Option<serde_json::Value>,
}

/// One `guard-audit/v1` record before chaining (MILESTONE_18 §12.1). The
/// sink assigns `seq`, `prev_hash`, and `hash` (#352). Never contains
/// credentials, session handles, raw argument values outside the
/// operation's allowlist, or resource content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AuditRecord {
    pub record: RecordKind,
    pub decision_id: DecisionId,
    pub time: Timestamp,
    pub duration_us: Durations,
    pub identity: Option<AuditIdentity>,
    pub action: Option<ActionId>,
    pub resource: Option<EntityUid>,
    /// Hex SHA-256 of the validated arguments.
    pub args_digest: Option<String>,
    /// Only the operation's allowlisted fields.
    pub args_logged: serde_json::Map<String, serde_json::Value>,
    pub decision: Option<DecisionKind>,
    pub reason: Option<DenyReason>,
    pub determining_policies: Vec<PolicyId>,
    pub policy_set_hash: Option<String>,
    pub schema_hash: Option<String>,
    pub approval_id: Option<ApprovalId>,
    pub enforcement: Option<Enforcement>,
    pub outcome: Option<Outcome>,
    /// Business-API effect id, for ledger correlation.
    pub downstream_ref: Option<String>,
    pub untrusted: UntrustedCorrelation,
}

impl AuditRecord {
    /// An empty record of `kind` for `decision_id`; the pipeline fills it.
    pub fn new(record: RecordKind, decision_id: DecisionId, time: Timestamp) -> Self {
        Self {
            record,
            decision_id,
            time,
            duration_us: Durations::default(),
            identity: None,
            action: None,
            resource: None,
            args_digest: None,
            args_logged: serde_json::Map::new(),
            decision: None,
            reason: None,
            determining_policies: Vec::new(),
            policy_set_hash: None,
            schema_hash: None,
            approval_id: None,
            enforcement: None,
            outcome: None,
            downstream_ref: None,
            untrusted: UntrustedCorrelation::default(),
        }
    }
}

/// Where a record landed in the chain. `head_hash` is returned to clients
/// so they can anchor the log (MILESTONE_18 §12.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditReceipt {
    pub seq: u64,
    pub head_hash: String,
}

#[derive(Debug, thiserror::Error)]
pub enum AuditError {
    #[error("audit log io: {0}")]
    Io(#[from] std::io::Error),
    #[error("audit log unavailable: {0}")]
    Unavailable(String),
}

/// Durable, append-only audit log (#352). `append` must resolve only after
/// the record is durable; on `Err` the pipeline does not execute.
pub trait AuditSink: Send + Sync {
    fn append<'a>(
        &'a self,
        record: &'a AuditRecord,
    ) -> crate::BoxFuture<'a, Result<AuditReceipt, AuditError>>;
}
