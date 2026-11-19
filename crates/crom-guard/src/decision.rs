use std::fmt;

use serde::{Deserialize, Serialize};

use crate::ids::{GrantId, PolicyId};

/// Why a request was denied. Every variant means the executor does not run.
/// Only `code()` is shown to agents: it never reveals whether a resource
/// exists in another tenant, nor ancestor grant ids. The full value
/// (including `ChainDeny { grant_id }`) is for the audit log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
#[non_exhaustive]
pub enum DenyReason {
    /// Missing, malformed, or unknown session handle.
    NoGrant,
    GrantNotYetValid,
    GrantExpired,
    GrantRevoked,
    /// The tool name maps to no registered protected operation.
    UnknownAction,
    /// The action is registered but not in some grant of the chain
    /// (ADR 0003 §4.3).
    ActionNotGranted,
    /// Arguments failed schema validation or typed parsing.
    InvalidArguments,
    /// Not found, or found in another tenant; the two are indistinguishable.
    ResourceNotFound,
    /// A Cedar policy denied, or no policy permitted.
    PolicyDeny,
    /// Cedar reported an evaluation error; denied even if the decision was
    /// Allow (MILESTONE_18 §9.2).
    PolicyError,
    /// The resolver, policy engine, store, or broker was unavailable.
    EngineUnavailable,
    /// An ancestor grant in the delegation chain denied.
    ChainDeny {
        grant_id: GrantId,
    },
    /// A presented approval did not match (digest, grant, expiry, or
    /// already used).
    ApprovalInvalid,
    /// The decision record could not be written, so nothing executed
    /// (MILESTONE_18 §12.3).
    AuditUnavailable,
}

impl DenyReason {
    pub fn code(&self) -> &'static str {
        match self {
            Self::NoGrant => "no_grant",
            Self::GrantNotYetValid => "grant_not_yet_valid",
            Self::GrantExpired => "grant_expired",
            Self::GrantRevoked => "grant_revoked",
            Self::UnknownAction => "unknown_action",
            Self::ActionNotGranted => "action_not_granted",
            Self::InvalidArguments => "invalid_arguments",
            Self::ResourceNotFound => "resource_not_found",
            Self::PolicyDeny => "policy_deny",
            Self::PolicyError => "policy_error",
            Self::EngineUnavailable => "engine_unavailable",
            Self::ChainDeny { .. } => "chain_deny",
            Self::ApprovalInvalid => "approval_invalid",
            Self::AuditUnavailable => "audit_unavailable",
        }
    }
}

impl fmt::Display for DenyReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

/// SHA-256 over the canonical (grant id, action, resolved resource uid,
/// validated arguments). Computed in #354.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ActionDigest([u8; 32]);

impl ActionDigest {
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }

    pub fn from_hex(s: &str) -> Option<Self> {
        if s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let mut out = [0u8; 32];
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
        }
        Some(Self(out))
    }
}

impl fmt::Debug for ActionDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ActionDigest({})", self.to_hex())
    }
}

impl Serialize for ActionDigest {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for ActionDigest {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::from_hex(&s).ok_or_else(|| serde::de::Error::custom("expected 64 hex digits"))
    }
}

/// What an approval must satisfy for a retried request to be allowable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalRequirement {
    /// The `@approval("<role>")` value of the permit that would allow.
    pub role: String,
    pub action_digest: ActionDigest,
}

/// Result of authorization. Executors never receive a `Decision`; they
/// receive an [`AuthorizedAction`](crate::AuthorizedAction), which the
/// pipeline builds only from `Allow`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Decision {
    Allow {
        determining_policies: Vec<PolicyId>,
    },
    Deny {
        reason: DenyReason,
        determining_policies: Vec<PolicyId>,
    },
    /// Not an authorization: the request is denied now, and an approval
    /// matching `requirement` would make a *new* request allowable.
    ApprovalRequired {
        requirement: ApprovalRequirement,
    },
}

impl Decision {
    pub fn deny(reason: DenyReason) -> Self {
        Self::Deny {
            reason,
            determining_policies: Vec::new(),
        }
    }

    pub fn is_allow(&self) -> bool {
        matches!(self, Self::Allow { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Digests round-trip through hex and serde; malformed hex is rejected.
    #[test]
    fn action_digest_hex() {
        let mut b = [0u8; 32];
        b[0] = 0xab;
        b[31] = 0x01;
        let d = ActionDigest::from_bytes(b);
        let hex = d.to_hex();
        assert_eq!(hex.len(), 64);
        assert!(hex.starts_with("ab") && hex.ends_with("01"));
        assert_eq!(ActionDigest::from_hex(&hex), Some(d));
        let json = serde_json::to_string(&d).unwrap();
        assert_eq!(serde_json::from_str::<ActionDigest>(&json).unwrap(), d);
        for bad in ["", "ab", &"g".repeat(64), &"a".repeat(63), &"é".repeat(32)] {
            assert_eq!(ActionDigest::from_hex(bad), None, "{bad:?}");
        }
    }

    /// Only `Allow` reports `is_allow`; `ApprovalRequired` is not an allow.
    #[test]
    fn only_allow_is_allow() {
        let req = ApprovalRequirement {
            role: "supervisor".into(),
            action_digest: ActionDigest::from_bytes([0; 32]),
        };
        assert!(Decision::Allow {
            determining_policies: vec![]
        }
        .is_allow());
        assert!(!Decision::deny(DenyReason::PolicyDeny).is_allow());
        assert!(!Decision::ApprovalRequired { requirement: req }.is_allow());
    }

    /// Reason codes are stable snake_case strings, and the same code is used for not-found and other-tenant.
    #[test]
    fn deny_reason_codes() {
        assert_eq!(DenyReason::ResourceNotFound.code(), "resource_not_found");
        assert_eq!(
            DenyReason::ChainDeny {
                grant_id: GrantId::new("g1").unwrap()
            }
            .code(),
            "chain_deny"
        );
        let v = serde_json::to_value(DenyReason::GrantExpired).unwrap();
        assert_eq!(v, serde_json::json!({"reason": "grant_expired"}));
    }
}
