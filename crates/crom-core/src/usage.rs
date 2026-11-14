//! Provider-normalized token usage.
//!
//! Every quantity is `Option<u64>`: `None` means the provider did not report
//! it, which is distinct from a reported `Some(0)`. Normalization across
//! providers (each provider crate maps its wire format onto this):
//!
//! - `input_tokens`: prompt tokens billed at the regular input rate, i.e.
//!   **excluding** `cache_read_input_tokens` and `cache_creation_input_tokens`
//!   (Anthropic convention; OpenAI/Gemini/DeepSeek prompt totals are split).
//! - `output_tokens`: all generated tokens, **including** `reasoning_tokens`.
//! - `reasoning_tokens`: the reasoning/thinking subset of `output_tokens`.
//!
//! Aggregation is strict: a summed quantity is `None` if any contributing
//! value is `None`, so an unknown never silently becomes zero.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TokenUsage {
    #[serde(default)]
    pub input_tokens: Option<u64>,
    #[serde(default)]
    pub output_tokens: Option<u64>,
    #[serde(default)]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    pub cache_read_input_tokens: Option<u64>,
    #[serde(default)]
    pub reasoning_tokens: Option<u64>,
}

fn strict_sum(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    Some(a?.saturating_add(b?))
}

fn known_sum(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.saturating_add(y)),
        (x, None) => x,
        (None, y) => y,
    }
}

impl TokenUsage {
    /// Strictly add `other` into `self`: each field becomes `None` unless both
    /// sides know it.
    pub fn add(&mut self, other: &TokenUsage) {
        self.input_tokens = strict_sum(self.input_tokens, other.input_tokens);
        self.output_tokens = strict_sum(self.output_tokens, other.output_tokens);
        self.cache_creation_input_tokens = strict_sum(
            self.cache_creation_input_tokens,
            other.cache_creation_input_tokens,
        );
        self.cache_read_input_tokens =
            strict_sum(self.cache_read_input_tokens, other.cache_read_input_tokens);
        self.reasoning_tokens = strict_sum(self.reasoning_tokens, other.reasoning_tokens);
    }

    /// Best-effort add that sums only known values (unknown contributes
    /// nothing). For display totals only — never for cost or eval accounting.
    pub fn add_known(&mut self, other: &TokenUsage) {
        self.input_tokens = known_sum(self.input_tokens, other.input_tokens);
        self.output_tokens = known_sum(self.output_tokens, other.output_tokens);
        self.cache_creation_input_tokens = known_sum(
            self.cache_creation_input_tokens,
            other.cache_creation_input_tokens,
        );
        self.cache_read_input_tokens =
            known_sum(self.cache_read_input_tokens, other.cache_read_input_tokens);
        self.reasoning_tokens = known_sum(self.reasoning_tokens, other.reasoning_tokens);
    }

    /// Strictly combine two optional usage records: `None` if either is.
    pub fn combine(a: Option<&TokenUsage>, b: Option<&TokenUsage>) -> Option<TokenUsage> {
        let mut out = a?.clone();
        out.add(b?);
        Some(out)
    }
}

/// Strict running total over model round trips.
///
/// `total()` is `None` when no turn was recorded or when any recorded turn
/// reported no usage; otherwise each field is summed strictly (see
/// [`TokenUsage::add`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageTotals {
    turns: u32,
    total: Option<TokenUsage>,
}

impl UsageTotals {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&mut self, turn: Option<&TokenUsage>) {
        self.total = if self.turns == 0 {
            turn.cloned()
        } else {
            TokenUsage::combine(self.total.as_ref(), turn)
        };
        self.turns = self.turns.saturating_add(1);
    }

    /// Number of recorded round trips (including ones with unknown usage).
    pub fn turns(&self) -> u32 {
        self.turns
    }

    pub fn total(&self) -> Option<TokenUsage> {
        self.total.clone()
    }
}

impl<'a> FromIterator<Option<&'a TokenUsage>> for UsageTotals {
    fn from_iter<I: IntoIterator<Item = Option<&'a TokenUsage>>>(iter: I) -> Self {
        let mut t = UsageTotals::new();
        for u in iter {
            t.record(u);
        }
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full(i: u64, o: u64) -> TokenUsage {
        TokenUsage {
            input_tokens: Some(i),
            output_tokens: Some(o),
            cache_creation_input_tokens: Some(0),
            cache_read_input_tokens: Some(0),
            reasoning_tokens: Some(0),
        }
    }

    // Accumulating near u64::MAX saturates at u64::MAX instead of panicking or wrapping.
    #[test]
    fn add_saturates_on_overflow() {
        let mut base = full(u64::MAX - 1, u64::MAX);
        base.add(&full(10, 10));
        assert_eq!(base.input_tokens, Some(u64::MAX));
        assert_eq!(base.output_tokens, Some(u64::MAX));
    }

    // Known fields sum exactly; a field unknown on either side becomes None.
    #[test]
    fn add_is_strict_per_field() {
        let mut base = TokenUsage {
            input_tokens: Some(5),
            output_tokens: Some(7),
            cache_creation_input_tokens: Some(3),
            cache_read_input_tokens: None,
            reasoning_tokens: Some(1),
        };
        base.add(&TokenUsage {
            input_tokens: Some(1),
            output_tokens: None,
            cache_creation_input_tokens: Some(4),
            cache_read_input_tokens: Some(9),
            reasoning_tokens: Some(2),
        });
        assert_eq!(base.input_tokens, Some(6));
        assert_eq!(base.output_tokens, None);
        assert_eq!(base.cache_creation_input_tokens, Some(7));
        assert_eq!(base.cache_read_input_tokens, None);
        assert_eq!(base.reasoning_tokens, Some(3));
    }

    // add_known sums only reported values and keeps None only when both sides are unknown.
    #[test]
    fn add_known_ignores_unknowns() {
        let mut base = TokenUsage::default();
        base.add_known(&TokenUsage {
            input_tokens: Some(4),
            ..Default::default()
        });
        base.add_known(&TokenUsage {
            input_tokens: Some(1),
            output_tokens: Some(2),
            ..Default::default()
        });
        assert_eq!(base.input_tokens, Some(5));
        assert_eq!(base.output_tokens, Some(2));
        assert_eq!(base.reasoning_tokens, None);
    }

    // A run with no recorded turns has unknown (None) total usage.
    #[test]
    fn totals_empty_is_none() {
        let t = UsageTotals::new();
        assert_eq!(t.turns(), 0);
        assert_eq!(t.total(), None);
    }

    // One turn without usage poisons the whole total to None, regardless of order.
    #[test]
    fn totals_none_if_any_turn_unknown() {
        let a = full(10, 2);
        let t: UsageTotals = [Some(&a), None, Some(&a)].into_iter().collect();
        assert_eq!(t.turns(), 3);
        assert_eq!(t.total(), None);
        let t: UsageTotals = [None, Some(&a)].into_iter().collect();
        assert_eq!(t.total(), None);
    }

    // Fully reported turns sum field by field.
    #[test]
    fn totals_sum_known_turns() {
        let t: UsageTotals = [Some(&full(10, 2)), Some(&full(5, 3))]
            .into_iter()
            .collect();
        assert_eq!(t.turns(), 2);
        assert_eq!(t.total(), Some(full(15, 5)));
    }

    // Legacy records with numeric input/output and no reasoning field still deserialize.
    #[test]
    fn deserializes_legacy_shape() {
        let u: TokenUsage = serde_json::from_str(
            r#"{"input_tokens":3,"output_tokens":4,"cache_creation_input_tokens":null,"cache_read_input_tokens":null}"#,
        )
        .unwrap();
        assert_eq!(u.input_tokens, Some(3));
        assert_eq!(u.output_tokens, Some(4));
        assert_eq!(u.reasoning_tokens, None);
    }

    // Unknown quantities serialize as explicit null, never 0.
    #[test]
    fn unknown_serializes_as_null() {
        let v = serde_json::to_value(TokenUsage::default()).unwrap();
        assert!(v["input_tokens"].is_null());
        assert!(v["output_tokens"].is_null());
        assert!(v["reasoning_tokens"].is_null());
    }
}
