//! Token and nominal-cost budgets checked by the agent loop between model
//! turns (#282).
//!
//! Fail-closed: when a budget is set but the quantity it caps is unknown
//! (unreported usage, or no pricing for a cost cap), the run stops with
//! [`ResultSubtype::ErrorBudgetUsageUnknown`] instead of continuing
//! unmetered.

use serde::{Deserialize, Serialize};

use crate::events::ResultSubtype;
use crate::telemetry::ModelPricing;
use crate::usage::TokenUsage;

/// Cumulative per-run caps. `None` fields are unlimited.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageBudget {
    /// Cap on prompt tokens: `input_tokens` plus reported cache-read and
    /// cache-write tokens, summed over every round trip.
    pub max_input_tokens: Option<u64>,
    /// Cap on output tokens (including reasoning) over every round trip.
    pub max_output_tokens: Option<u64>,
    /// Cap on nominal list-price cost in USD.
    pub max_cost_usd: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetKind {
    InputTokens,
    OutputTokens,
    Cost,
}

/// Why a run was stopped by a [`UsageBudget`].
#[derive(Debug, Clone, PartialEq)]
pub struct BudgetBreach {
    pub kind: BudgetKind,
    /// The capped quantity was unknown, so the budget could not be checked.
    pub unknown: bool,
    pub message: String,
}

impl BudgetBreach {
    pub fn subtype(&self) -> ResultSubtype {
        if self.unknown {
            return ResultSubtype::ErrorBudgetUsageUnknown;
        }
        match self.kind {
            BudgetKind::InputTokens => ResultSubtype::ErrorMaxInputTokens,
            BudgetKind::OutputTokens => ResultSubtype::ErrorMaxOutputTokens,
            BudgetKind::Cost => ResultSubtype::ErrorMaxCost,
        }
    }

    fn unknown(kind: BudgetKind, what: &str) -> Self {
        Self {
            kind,
            unknown: true,
            message: format!("{what} is unknown, so the budget cannot be enforced"),
        }
    }
}

fn prompt_tokens(u: &TokenUsage) -> Option<u64> {
    Some(
        u.input_tokens?
            .saturating_add(u.cache_read_input_tokens.unwrap_or(0))
            .saturating_add(u.cache_creation_input_tokens.unwrap_or(0)),
    )
}

impl UsageBudget {
    pub fn is_unlimited(&self) -> bool {
        self.max_input_tokens.is_none()
            && self.max_output_tokens.is_none()
            && self.max_cost_usd.is_none()
    }

    /// Checked before the first model call: a cost cap with no price can
    /// never be enforced, so the run must not start.
    pub fn preflight(&self, pricing: Option<&ModelPricing>) -> Option<BudgetBreach> {
        if self.max_cost_usd.is_some() && pricing.is_none() {
            return Some(BudgetBreach::unknown(
                BudgetKind::Cost,
                "model price (no pricing entry)",
            ));
        }
        None
    }

    /// Checked after each model turn against the strict run total
    /// (`None` = some round trip reported no usage). A quantity equal to its
    /// cap is within budget; strictly greater is a breach.
    pub fn check(
        &self,
        total: Option<&TokenUsage>,
        pricing: Option<&ModelPricing>,
    ) -> Option<BudgetBreach> {
        if self.is_unlimited() {
            return None;
        }
        if let Some(max) = self.max_input_tokens {
            match total.and_then(prompt_tokens) {
                None => {
                    return Some(BudgetBreach::unknown(
                        BudgetKind::InputTokens,
                        "input token usage",
                    ))
                }
                Some(n) if n > max => {
                    return Some(BudgetBreach {
                        kind: BudgetKind::InputTokens,
                        unknown: false,
                        message: format!("{n} input tokens exceeds max_input_tokens={max}"),
                    })
                }
                Some(_) => {}
            }
        }
        if let Some(max) = self.max_output_tokens {
            match total.and_then(|u| u.output_tokens) {
                None => {
                    return Some(BudgetBreach::unknown(
                        BudgetKind::OutputTokens,
                        "output token usage",
                    ))
                }
                Some(n) if n > max => {
                    return Some(BudgetBreach {
                        kind: BudgetKind::OutputTokens,
                        unknown: false,
                        message: format!("{n} output tokens exceeds max_output_tokens={max}"),
                    })
                }
                Some(_) => {}
            }
        }
        if let Some(max) = self.max_cost_usd {
            let Some(pricing) = pricing else {
                return Some(BudgetBreach::unknown(
                    BudgetKind::Cost,
                    "model price (no pricing entry)",
                ));
            };
            let cost = match total {
                None => Err("usage_unknown"),
                Some(u) => pricing.estimate_explained(u).map_err(|r| r.key()),
            };
            match cost {
                Err(reason) => {
                    return Some(BudgetBreach::unknown(
                        BudgetKind::Cost,
                        &format!("nominal cost ({reason})"),
                    ))
                }
                Ok(c) if c > max => {
                    return Some(BudgetBreach {
                        kind: BudgetKind::Cost,
                        unknown: false,
                        message: format!("nominal cost ${c:.6} exceeds max_cost_usd=${max:.6}"),
                    })
                }
                Ok(_) => {}
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(i: Option<u64>, o: Option<u64>) -> TokenUsage {
        TokenUsage {
            input_tokens: i,
            output_tokens: o,
            ..Default::default()
        }
    }

    fn flat(input: f64, output: f64) -> ModelPricing {
        ModelPricing {
            input_per_million: input,
            output_per_million: output,
            cache_creation_per_million: None,
            cache_read_per_million: None,
        }
    }

    // An unlimited budget never breaches, even with unknown usage.
    #[test]
    fn unlimited_never_breaches() {
        assert_eq!(UsageBudget::default().check(None, None), None);
    }

    // Token caps: equal is within budget, greater is a breach of the matching kind.
    #[test]
    fn token_caps_trip_strictly_above_max() {
        let b = UsageBudget {
            max_input_tokens: Some(100),
            max_output_tokens: Some(10),
            ..Default::default()
        };
        assert_eq!(b.check(Some(&usage(Some(100), Some(10))), None), None);
        let over_in = b.check(Some(&usage(Some(101), Some(1))), None).unwrap();
        assert_eq!(over_in.subtype(), ResultSubtype::ErrorMaxInputTokens);
        let over_out = b.check(Some(&usage(Some(1), Some(11))), None).unwrap();
        assert_eq!(over_out.subtype(), ResultSubtype::ErrorMaxOutputTokens);
    }

    // Reported cache tokens count toward the input cap.
    #[test]
    fn input_cap_counts_cache_tokens() {
        let b = UsageBudget {
            max_input_tokens: Some(100),
            ..Default::default()
        };
        let u = TokenUsage {
            cache_read_input_tokens: Some(60),
            ..usage(Some(50), Some(0))
        };
        assert!(b.check(Some(&u), None).is_some());
    }

    // Cost cap: priced usage above the cap breaches; at or below does not.
    #[test]
    fn cost_cap_uses_nominal_price() {
        let b = UsageBudget {
            max_cost_usd: Some(1.0),
            ..Default::default()
        };
        let p = flat(1.0, 1.0);
        assert_eq!(
            b.check(Some(&usage(Some(500_000), Some(500_000))), Some(&p)),
            None
        );
        let br = b
            .check(Some(&usage(Some(500_000), Some(500_001))), Some(&p))
            .unwrap();
        assert_eq!(br.subtype(), ResultSubtype::ErrorMaxCost);
    }

    // A set budget with unknown usage or no price fails closed.
    #[test]
    fn unknown_usage_or_price_fails_closed() {
        let cost = UsageBudget {
            max_cost_usd: Some(1.0),
            ..Default::default()
        };
        let p = flat(1.0, 1.0);
        for br in [
            cost.check(None, Some(&p)),
            cost.check(Some(&usage(None, Some(1))), Some(&p)),
            cost.check(Some(&usage(Some(1), Some(1))), None),
            cost.preflight(None),
        ] {
            assert_eq!(
                br.unwrap().subtype(),
                ResultSubtype::ErrorBudgetUsageUnknown
            );
        }
        assert_eq!(cost.preflight(Some(&p)), None);
        let out = UsageBudget {
            max_output_tokens: Some(5),
            ..Default::default()
        };
        assert_eq!(
            out.check(Some(&usage(Some(1), None)), None)
                .unwrap()
                .subtype(),
            ResultSubtype::ErrorBudgetUsageUnknown
        );
    }
}
