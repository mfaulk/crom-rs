//! Identifier newtypes. An identifier names something; it never carries
//! authority, so every id type here is freely parseable. Authority comes only
//! from an [`ExecutionContext`](crate::ExecutionContext).

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Maximum identifier length in bytes.
pub const MAX_ID_LEN: usize = 128;

/// Why a string was rejected as an identifier.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdError {
    #[error("{kind} is empty")]
    Empty { kind: &'static str },
    #[error("{kind} is longer than {MAX_ID_LEN} bytes")]
    TooLong { kind: &'static str },
    #[error("{kind} contains {found:?}; allowed: ASCII letters, digits, and . _ - : @")]
    InvalidChar { kind: &'static str, found: char },
    #[error("{kind} uses the prefix {RESERVED_ID_PREFIX:?}, which is reserved for Guard")]
    Reserved { kind: &'static str },
}

/// Prefix of Guard-internal identifiers, such as the approval-probe
/// placeholder user (ADR 0003 §4.4). No externally supplied id may use it.
pub const RESERVED_ID_PREFIX: &str = "__guard";

/// Allowed identifier bytes: ASCII alphanumerics and `. _ - : @`. The set
/// excludes whitespace, quotes, `/`, and control characters, so an id can
/// be placed in a URL path segment, a header, a log line, or a Cedar entity
/// id without escaping.
fn validate(kind: &'static str, s: &str) -> Result<(), IdError> {
    if s.is_empty() {
        return Err(IdError::Empty { kind });
    }
    if s.len() > MAX_ID_LEN {
        return Err(IdError::TooLong { kind });
    }
    if let Some(found) = s
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':' | '@')))
    {
        return Err(IdError::InvalidChar { kind, found });
    }
    if s.starts_with(RESERVED_ID_PREFIX) {
        return Err(IdError::Reserved { kind });
    }
    Ok(())
}

macro_rules! id_type {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(String);

        impl $name {
            /// Validates `s` (see [`MAX_ID_LEN`] and the allowed charset).
            pub fn new(s: impl Into<String>) -> Result<Self, IdError> {
                let s = s.into();
                validate(stringify!($name), &s)?;
                Ok(Self(s))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl std::str::FromStr for $name {
            type Err = IdError;
            fn from_str(s: &str) -> Result<Self, IdError> {
                Self::new(s)
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let s = String::deserialize(d)?;
                Self::new(s).map_err(serde::de::Error::custom)
            }
        }
    };
}

id_type!(
    /// Guard-generated grant id.
    GrantId
);
id_type!(
    /// Guard-generated agent instance id, one per grant chain root.
    AgentId
);
id_type!(
    /// Run or task id supplied by the trusted application.
    RunId
);
id_type!(
    /// Guard-generated id of one authorization decision; also sent
    /// downstream as `X-Guard-Decision-Id`.
    DecisionId
);
id_type!(
    /// Guard-generated approval id.
    ApprovalId
);
id_type!(
    /// Protected-operation id, e.g. `refund.issue`. Exactly one per MCP tool.
    ActionId
);
id_type!(
    /// Tenant id, asserted by the trusted application at grant issuance.
    TenantId
);
id_type!(
    /// Delegating user id, asserted by the trusted application.
    UserId
);
id_type!(
    /// Trusted application (grant issuer) id, from Guard's configuration.
    AppId
);
id_type!(
    /// Policy id: the `@id` annotation of a Cedar policy (ADR 0003 §4.4).
    PolicyId
);

#[cfg(test)]
mod tests {
    use super::*;

    /// Ids made only of the allowed charset are accepted and round-trip through serde.
    #[test]
    fn valid_ids_round_trip() {
        for s in [
            "refund.issue",
            "g_01HX",
            "acme-cus-001",
            "app:support@v1",
            "A",
        ] {
            let id = ActionId::new(s).unwrap();
            assert_eq!(id.as_str(), s);
            let json = serde_json::to_string(&id).unwrap();
            assert_eq!(serde_json::from_str::<ActionId>(&json).unwrap(), id);
        }
    }

    /// Empty, overlong, and out-of-charset ids are rejected, including through serde.
    #[test]
    fn invalid_ids_rejected() {
        assert!(matches!(TenantId::new(""), Err(IdError::Empty { .. })));
        assert!(matches!(
            TenantId::new("a".repeat(MAX_ID_LEN + 1)),
            Err(IdError::TooLong { .. })
        ));
        assert!(TenantId::new("a".repeat(MAX_ID_LEN)).is_ok());
        assert!(matches!(
            UserId::new("__guard_approval_placeholder__"),
            Err(IdError::Reserved { .. })
        ));
        assert!(serde_json::from_str::<UserId>("\"__guard_x\"").is_err());
        assert!(UserId::new("_guard").is_ok());
        for bad in ["acme corp", "a/b", "a\"b", "a\nb", "é", "a\\b", "a\0b"] {
            assert!(
                matches!(TenantId::new(bad), Err(IdError::InvalidChar { .. })),
                "{bad:?}"
            );
            let json = serde_json::to_string(bad).unwrap();
            assert!(serde_json::from_str::<TenantId>(&json).is_err(), "{bad:?}");
        }
    }
}
