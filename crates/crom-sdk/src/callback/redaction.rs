use regex::Regex;
use std::sync::OnceLock;

use super::SecretRedactor;

/// No-op redactor. Useful in tests.
pub struct NoopSecretRedactor;

impl SecretRedactor for NoopSecretRedactor {
    fn redact(&self, input: &str) -> String {
        input.to_string()
    }
}

/// Conservative default redactor that knocks out the most common secret
/// patterns. Not exhaustive — applications with strict requirements
/// should wire in their own.
pub struct DefaultSecretRedactor;

fn patterns() -> &'static [(Regex, &'static str)] {
    static P: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();
    P.get_or_init(|| {
        vec![
            // Anthropic style keys
            (
                Regex::new(r"sk-ant-[A-Za-z0-9_\-]{20,}").unwrap(),
                "<redacted:anthropic-api-key>",
            ),
            // OpenAI style keys
            (
                Regex::new(r"sk-[A-Za-z0-9_\-]{20,}").unwrap(),
                "<redacted:openai-api-key>",
            ),
            // Generic bearer tokens
            (
                Regex::new(r"(?i)bearer\s+[A-Za-z0-9_\-\.]{20,}").unwrap(),
                "<redacted:bearer>",
            ),
            // AWS access keys
            (
                Regex::new(r"AKIA[0-9A-Z]{16}").unwrap(),
                "<redacted:aws-access-key>",
            ),
            // Generic high-entropy tokens following common prefixes.
            // Capture the label so it stays in the output; the secret value
            // itself is replaced. Using `$0` here would leak the value.
            (
                Regex::new(r"(?i)(api[_-]?key|token|secret)\s*[:=]\s*[A-Za-z0-9_\-]{16,}").unwrap(),
                "$1=<redacted>",
            ),
        ]
    })
}

impl SecretRedactor for DefaultSecretRedactor {
    fn redact(&self, input: &str) -> String {
        let mut out = input.to_string();
        for (re, replacement) in patterns() {
            out = re.replace_all(&out, *replacement).into_owned();
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// DefaultSecretRedactor must remove (not just label) generic
    /// label-prefixed secrets. The earlier implementation used `$0` in the
    /// replacement, which preserved the secret verbatim.
    #[test]
    fn redacts_labelled_secret_value() {
        let r = DefaultSecretRedactor;
        let out = r.redact("api_key=ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789");
        assert!(!out.contains("ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789"));
        assert!(out.contains("<redacted>"));
        assert!(out.contains("api_key"));
    }

    /// DefaultSecretRedactor redacts a Claude-style API key.
    #[test]
    fn redacts_anthropic_key() {
        let r = DefaultSecretRedactor;
        let out = r.redact("token: sk-ant-abcdefghijklmnopqrstuvwxyz123");
        assert!(out.contains("<redacted"));
        assert!(!out.contains("abcdefghijkl"));
    }

    /// NoopSecretRedactor is a no-op.
    #[test]
    fn noop_passes_through() {
        let r = NoopSecretRedactor;
        assert_eq!(r.redact("hello"), "hello");
    }
}
