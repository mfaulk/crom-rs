use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
}

impl TokenUsage {
    pub fn add(&mut self, other: &TokenUsage) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        if let Some(v) = other.cache_creation_input_tokens {
            let slot = self.cache_creation_input_tokens.get_or_insert(0);
            *slot = slot.saturating_add(v);
        }
        if let Some(v) = other.cache_read_input_tokens {
            let slot = self.cache_read_input_tokens.get_or_insert(0);
            *slot = slot.saturating_add(v);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Accumulating near u64::MAX saturates at u64::MAX instead of panicking or wrapping.
    #[test]
    fn add_saturates_on_overflow() {
        let mut base = TokenUsage {
            input_tokens: u64::MAX - 1,
            output_tokens: u64::MAX,
            cache_creation_input_tokens: Some(u64::MAX - 2),
            cache_read_input_tokens: None,
        };
        let other = TokenUsage {
            input_tokens: 10,
            output_tokens: 10,
            cache_creation_input_tokens: Some(10),
            cache_read_input_tokens: Some(u64::MAX),
        };
        base.add(&other);
        assert_eq!(base.input_tokens, u64::MAX);
        assert_eq!(base.output_tokens, u64::MAX);
        assert_eq!(base.cache_creation_input_tokens, Some(u64::MAX));
        // get_or_insert(0) starts at 0, then saturates with u64::MAX.
        assert_eq!(base.cache_read_input_tokens, Some(u64::MAX));
    }

    // Normal addition without overflow sums fields exactly.
    #[test]
    fn add_sums_without_overflow() {
        let mut base = TokenUsage {
            input_tokens: 5,
            output_tokens: 7,
            cache_creation_input_tokens: Some(3),
            cache_read_input_tokens: None,
        };
        base.add(&TokenUsage {
            input_tokens: 1,
            output_tokens: 2,
            cache_creation_input_tokens: Some(4),
            cache_read_input_tokens: Some(9),
        });
        assert_eq!(base.input_tokens, 6);
        assert_eq!(base.output_tokens, 9);
        assert_eq!(base.cache_creation_input_tokens, Some(7));
        assert_eq!(base.cache_read_input_tokens, Some(9));
    }
}
