/// Prefix of every session handle; lets secret scanners recognise one.
pub const HANDLE_PREFIX: &str = "gsh_";

/// The 256-bit bearer value an agent presents as
/// `Authorization: Bearer <handle>`: `gsh_` followed by 64 lowercase hex
/// digits. It is a lookup key with no embedded authority; Guard stores only
/// its hash (#348).
///
/// No `Debug`, `Display`, `Serialize`, or `Clone`, so it cannot be logged or
/// copied by accident:
///
/// ```compile_fail
/// let h = crom_guard::SessionHandle::parse("gsh_00").unwrap();
/// println!("{:?}", h);
/// ```
///
/// ```compile_fail
/// fn copy(h: &crom_guard::SessionHandle) -> crom_guard::SessionHandle { h.clone() }
/// ```
///
/// ```compile_fail
/// fn ser(h: &crom_guard::SessionHandle) -> String { serde_json::to_string(h).unwrap() }
/// ```
#[allow(missing_debug_implementations)] // holds a secret; deliberately not Debug
pub struct SessionHandle {
    secret: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("malformed session handle")]
pub struct HandleError;

impl SessionHandle {
    /// Parses a presented bearer value. Malformed input is rejected before
    /// any store lookup; the error never echoes the input.
    pub fn parse(bearer: &str) -> Result<Self, HandleError> {
        let hex = bearer.strip_prefix(HANDLE_PREFIX).ok_or(HandleError)?;
        if hex.len() != 64 || !hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return Err(HandleError);
        }
        Ok(Self {
            secret: bearer.to_owned(),
        })
    }

    /// Builds a handle from fresh random bytes (the store's generator, #348).
    #[allow(dead_code)] // used by the grant store (#348)
    pub(crate) fn from_random_bytes(bytes: [u8; 32]) -> Self {
        let mut secret = String::with_capacity(HANDLE_PREFIX.len() + 64);
        secret.push_str(HANDLE_PREFIX);
        for b in bytes {
            secret.push_str(&format!("{b:02x}"));
        }
        Self { secret }
    }

    /// The bearer value. Use only to hand the handle to the trusted
    /// application at issuance, or to hash it for lookup.
    pub fn expose_secret(&self) -> &str {
        &self.secret
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Generated handles parse back; anything but `gsh_` plus 64 lowercase hex digits is rejected.
    #[test]
    fn handle_format() {
        let h = SessionHandle::from_random_bytes([0xa5; 32]);
        assert_eq!(h.expose_secret().len(), 68);
        let parsed = SessionHandle::parse(h.expose_secret()).unwrap();
        assert_eq!(parsed.expose_secret(), h.expose_secret());
        let good = format!("gsh_{}", "0".repeat(64));
        assert!(SessionHandle::parse(&good).is_ok());
        for bad in [
            String::new(),
            "0".repeat(64),
            format!("gsh_{}", "0".repeat(63)),
            format!("gsh_{}", "0".repeat(65)),
            format!("gsh_{}", "A".repeat(64)),
            format!("GSH_{}", "0".repeat(64)),
            format!(" gsh_{}", "0".repeat(64)),
        ] {
            assert!(SessionHandle::parse(&bad).is_err(), "{bad:?}");
        }
    }
}
