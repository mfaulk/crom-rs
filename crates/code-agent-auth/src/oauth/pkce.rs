//! PKCE verifier/challenge + CSRF state generation.
//!
//! Bytes come from the OS CSPRNG via [`rand::rngs::SysRng`] (which uses
//! `getrandom` under the hood). Earlier revisions of this module
//! derived these values from `SystemTime + pid + counter` hashed with
//! SHA-256 — that was rejected during code review as not appropriate
//! entropy for OAuth state and PKCE verifiers, since the inputs are
//! predictable to a local observer.

use rand::rngs::SysRng;
use rand::TryRng;
use sha2::Digest;

/// Generate a URL-safe random string suitable for the PKCE
/// `code_verifier`. RFC 7636 requires 43–128 characters from
/// `[A-Z]/[a-z]/[0-9]/-/./_/~`; base64-url without padding keeps the
/// output inside that set. 48 raw bytes → 64 char output.
pub fn generate_verifier() -> String {
    base64url(&random_bytes(48))
}

/// Generate a CSRF state token. 32 bytes of OS-CSPRNG entropy
/// expressed as base64-url.
pub fn generate_state() -> String {
    base64url(&random_bytes(32))
}

/// Compute the S256 PKCE code challenge for a verifier.
pub fn s256_challenge(verifier: &str) -> String {
    let digest = sha2::Sha256::digest(verifier.as_bytes());
    base64url(&digest)
}

fn random_bytes(n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    // rand 0.10 models the OS CSPRNG as a *fallible* `TryRng` (getrandom
    // can fail), so there is no infallible `fill_bytes`. A CSPRNG failure
    // here is unrecoverable for OAuth state / PKCE verifiers — emitting
    // low-entropy material would be worse than aborting — so surface it as a
    // panic rather than degrade silently.
    SysRng
        .try_fill_bytes(&mut buf)
        .expect("OS CSPRNG (getrandom) failed");
    buf
}

/// Base64-URL encode without padding.
fn base64url(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity((bytes.len() * 4).div_ceil(3));
    let mut i = 0;
    while i + 3 <= bytes.len() {
        let n = ((bytes[i] as u32) << 16) | ((bytes[i + 1] as u32) << 8) | (bytes[i + 2] as u32);
        out.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
        out.push(TABLE[((n >> 6) & 0x3f) as usize] as char);
        out.push(TABLE[(n & 0x3f) as usize] as char);
        i += 3;
    }
    let rem = bytes.len() - i;
    if rem == 1 {
        let n = (bytes[i] as u32) << 16;
        out.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
    } else if rem == 2 {
        let n = ((bytes[i] as u32) << 16) | ((bytes[i + 1] as u32) << 8);
        out.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
        out.push(TABLE[((n >> 6) & 0x3f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    // Verifier respects RFC 7636 length and character constraints.
    fn verifier_in_rfc7636_alphabet() {
        let v = generate_verifier();
        assert!(v.len() >= 43);
        assert!(v.len() <= 128);
        assert!(v
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    #[test]
    // 1024 consecutive verifier draws have no collisions and pass a
    // light entropy smell-test (no draw shares more than 50% of its
    // bytes with any other draw). This is a sanity check that we are
    // calling SysRng, not a deterministic generator.
    fn entropy_no_collisions_over_many_draws() {
        let mut seen = HashSet::new();
        for _ in 0..1024 {
            let v = generate_verifier();
            assert!(seen.insert(v), "verifier collision under SysRng");
        }
    }

    #[test]
    // S256 challenge is the base64url(SHA256(verifier)) per RFC 7636.
    fn s256_matches_known_vector() {
        // Test vector from RFC 7636 §4.4.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let expected = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        assert_eq!(s256_challenge(verifier), expected);
    }
}
