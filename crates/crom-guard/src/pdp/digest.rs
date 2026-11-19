use sha2::{Digest, Sha256};

use crate::action::ValidatedArgs;
use crate::decision::ActionDigest;
use crate::ids::{ActionId, GrantId};
use crate::value::EntityUid;

/// Domain-separation prefix of the v1 action digest encoding.
pub const ACTION_DIGEST_DOMAIN: &[u8] = b"crom-guard/action-digest/v1\0";

impl ActionDigest {
    /// The digest an approval is bound to (MILESTONE_18 §9.4). The PDP uses
    /// it for `ApprovalRequired`, and the approval store (#354) must call
    /// this same function to match a retried request.
    ///
    /// Canonical encoding (v1). The digest is SHA-256 over:
    ///
    /// ```text
    /// ACTION_DIGEST_DOMAIN                 (b"crom-guard/action-digest/v1\0")
    /// field(grant_id)                      the leaf grant id
    /// field(action)
    /// field(resource.entity_type())        e.g. "Support::Invoice"
    /// field(resource.id())
    /// field(canonical_json(args))
    ///
    /// field(b) = len(b) as u64 big-endian (8 bytes) || b
    /// ```
    ///
    /// `canonical_json` is UTF-8 JSON with no whitespace; object members
    /// sorted by key, comparing the keys' UTF-8 bytes; strings (keys and
    /// values) escaped as `serde_json` escapes them (`"`, `\`, and control
    /// characters; other characters raw); numbers in `serde_json`'s
    /// `Number` display form; `null`, `true`, `false` literally. Array
    /// order is preserved.
    pub fn compute(
        grant_id: &GrantId,
        action: &ActionId,
        resource: &EntityUid,
        args: &ValidatedArgs,
    ) -> Self {
        let mut json = String::new();
        canonical_json(args.as_json(), &mut json);
        let mut h = Sha256::new();
        h.update(ACTION_DIGEST_DOMAIN);
        for field in [
            grant_id.as_str(),
            action.as_str(),
            resource.entity_type(),
            resource.id(),
            json.as_str(),
        ] {
            h.update((field.len() as u64).to_be_bytes());
            h.update(field.as_bytes());
        }
        Self::from_bytes(h.finalize().into())
    }
}

/// Hex SHA-256 of the canonical JSON of `args` (the encoding documented on
/// [`ActionDigest::compute`]): the audit `args_digest`.
pub(crate) fn args_digest_hex(args: &ValidatedArgs) -> String {
    let mut json = String::new();
    canonical_json(args.as_json(), &mut json);
    hex::encode(Sha256::digest(json.as_bytes()))
}

fn canonical_json(v: &serde_json::Value, out: &mut String) {
    use serde_json::Value;
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => push_json_string(s, out),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                canonical_json(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            // Sorted explicitly: `serde_json`'s map order depends on the
            // `preserve_order` feature, which any crate in a build can enable.
            let mut members: Vec<_> = map.iter().collect();
            members.sort_unstable_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
            out.push('{');
            for (i, (k, val)) in members.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                push_json_string(k, out);
                out.push(':');
                canonical_json(val, out);
            }
            out.push('}');
        }
    }
}

fn push_json_string(s: &str, out: &mut String) {
    out.push_str(&serde_json::to_string(s).expect("serializing a str cannot fail"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn digest(grant: &str, action: &str, ty: &str, id: &str, args: serde_json::Value) -> String {
        ActionDigest::compute(
            &GrantId::new(grant).unwrap(),
            &ActionId::new(action).unwrap(),
            &EntityUid::new(ty, id).unwrap(),
            &ValidatedArgs::new(args),
        )
        .to_hex()
    }

    /// The encoding matches an independently assembled byte string, pinning v1 for #354.
    #[test]
    fn digest_matches_documented_encoding() {
        let args = json!({"note": "a\"b", "amount_cents": 8000, "invoice_id": "i1"});
        let mut bytes = ACTION_DIGEST_DOMAIN.to_vec();
        for f in [
            "g1",
            "refund.issue",
            "Support::Invoice",
            "i1",
            r#"{"amount_cents":8000,"invoice_id":"i1","note":"a\"b"}"#,
        ] {
            bytes.extend((f.len() as u64).to_be_bytes());
            bytes.extend(f.as_bytes());
        }
        let expected = ActionDigest::from_bytes(Sha256::digest(&bytes).into()).to_hex();
        assert_eq!(
            digest("g1", "refund.issue", "Support::Invoice", "i1", args),
            expected
        );
    }

    /// Object member order and nesting order do not change the digest.
    #[test]
    fn digest_ignores_member_order() {
        let a: serde_json::Value =
            serde_json::from_str(r#"{"b":1,"a":{"y":[1,2],"x":null}}"#).unwrap();
        let b: serde_json::Value =
            serde_json::from_str(r#"{"a":{"x":null,"y":[1,2]},"b":1}"#).unwrap();
        assert_eq!(
            digest("g1", "refund.issue", "Support::Invoice", "i1", a),
            digest("g1", "refund.issue", "Support::Invoice", "i1", b)
        );
    }

    /// Changing any one input (grant, action, resource type or id, an argument, array order) changes the digest.
    #[test]
    fn digest_binds_every_field() {
        let base = || json!({"amount_cents": 8000, "tags": [1, 2]});
        let d = digest("g1", "refund.issue", "Support::Invoice", "i1", base());
        for other in [
            digest("g2", "refund.issue", "Support::Invoice", "i1", base()),
            digest("g1", "refund.void", "Support::Invoice", "i1", base()),
            digest("g1", "refund.issue", "Support::Customer", "i1", base()),
            digest("g1", "refund.issue", "Support::Invoice", "i2", base()),
            digest(
                "g1",
                "refund.issue",
                "Support::Invoice",
                "i1",
                json!({"amount_cents": 8001, "tags": [1, 2]}),
            ),
            digest(
                "g1",
                "refund.issue",
                "Support::Invoice",
                "i1",
                json!({"amount_cents": 8000, "tags": [2, 1]}),
            ),
        ] {
            assert_ne!(d, other);
        }
    }

    /// Length prefixes keep field boundaries unambiguous ("ab"+"c" differs from "a"+"bc").
    #[test]
    fn digest_fields_are_length_prefixed() {
        assert_ne!(
            digest("ab", "c", "Support::Invoice", "i1", json!({})),
            digest("a", "bc", "Support::Invoice", "i1", json!({}))
        );
    }
}
