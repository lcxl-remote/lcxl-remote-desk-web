//! Integer-only cost accounting for an independent approval-model call.
//! Prices are micro-units of the delegation's configured accounting currency
//! per one million tokens. Missing prices or usage cannot become a zero bill.

use serde::{Deserialize, Serialize};

use crate::chat::TokenUsage;

const TOKENS_PER_PRICE_UNIT: u128 = 1_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalTokenPrices {
    pub input_micros_per_million: u64,
    pub output_micros_per_million: u64,
    pub cache_read_micros_per_million: u64,
    pub cache_write_micros_per_million: u64,
}

impl ApprovalTokenPrices {
    pub fn validate(self) -> Option<Self> {
        (self.input_micros_per_million > 0 && self.output_micros_per_million > 0).then_some(self)
    }

    /// Reserve the full request context and the configured runtime output budget.
    /// Cache classes use the maximum of their own configured price and the
    /// ordinary input price because the provider may change cache behavior.
    pub fn reserve(self, max_input_tokens: u64, max_output_tokens: u64) -> Option<u64> {
        self.validate()?;
        let input_rate = self
            .input_micros_per_million
            .max(self.cache_read_micros_per_million)
            .max(self.cache_write_micros_per_million);
        rounded_cost(&[
            (max_input_tokens, input_rate),
            (max_output_tokens, self.output_micros_per_million),
        ])
    }

    /// Charge the four disjoint normalized token classes. Unknown base usage
    /// returns None so the caller retains the reserved upper-bound charge.
    pub fn actual(self, usage: TokenUsage) -> Option<u64> {
        self.validate()?;
        let input = u64::try_from(usage.input_tokens?).ok()?;
        let output = u64::try_from(usage.output_tokens?).ok()?;
        let cache_read = u64::try_from(usage.cache_read_tokens.unwrap_or(0)).ok()?;
        let cache_write = u64::try_from(usage.cache_write_tokens.unwrap_or(0)).ok()?;
        rounded_cost(&[
            (input, self.input_micros_per_million),
            (output, self.output_micros_per_million),
            (cache_read, self.cache_read_micros_per_million),
            (cache_write, self.cache_write_micros_per_million),
        ])
    }
}

/// The independent reviewer has no tools and sends only a fixed system prompt
/// plus the already authorized candidate prompt. Reserve a conservative UTF-8
/// byte bound for input tokens, wire framing, and the configured runtime output
/// budget. Oversized requests fail before any provider dial.
pub fn reviewer_reservation(
    authorized_prompt: &str,
    prices: ApprovalTokenPrices,
    model_context_bytes: u64,
    runtime_max_output_tokens: u64,
) -> Option<(u64, u64)> {
    let tokens = crate::approval_usage::reviewer_token_reservation(
        authorized_prompt,
        model_context_bytes,
        runtime_max_output_tokens,
    )?;
    let input_upper = tokens.checked_sub(runtime_max_output_tokens)?;
    let cost = prices.reserve(input_upper, runtime_max_output_tokens)?;
    (cost > 0).then_some((tokens, cost))
}

/// One record's contribution to the cumulative approval ledger. Unknown usage
/// holds both original bounds; known provider facts replace that contribution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewUsageSettlement {
    pub schema_version: u16,
    pub provider_started: bool,
    pub usage_known: bool,
    pub tokens: u64,
    pub cost_micros: u64,
}

impl ReviewUsageSettlement {
    pub fn from_provider_fact(
        provider_started: bool,
        tokens: Option<u64>,
        cost_micros: Option<u64>,
        reserved_tokens: u64,
        reserved_cost_micros: u64,
    ) -> Option<Self> {
        if reserved_tokens == 0 || reserved_cost_micros == 0 {
            return None;
        }
        let (tokens, cost_micros, usage_known) = if provider_started {
            match tokens.zip(cost_micros) {
                Some((tokens, cost)) => (tokens, cost, true),
                None => (reserved_tokens, reserved_cost_micros, false),
            }
        } else {
            if tokens.is_some_and(|tokens| tokens != 0) || cost_micros.is_some_and(|cost| cost != 0)
            {
                return None;
            }
            (0, 0, true)
        };
        let value = Self {
            schema_version: 1,
            provider_started,
            usage_known,
            tokens,
            cost_micros,
        };
        value.validate(reserved_tokens, reserved_cost_micros)?;
        Some(value)
    }

    pub fn state(self) -> &'static str {
        if !self.provider_started {
            "unstarted"
        } else if self.usage_known {
            "known"
        } else {
            "unknown"
        }
    }

    pub fn validate(self, reserved_tokens: u64, reserved_cost_micros: u64) -> Option<Self> {
        if self.schema_version != 1
            || reserved_tokens == 0
            || reserved_cost_micros == 0
            || !self.provider_started
                && (!self.usage_known || self.tokens != 0 || self.cost_micros != 0)
            || self.provider_started
                && !self.usage_known
                && (self.tokens != reserved_tokens || self.cost_micros != reserved_cost_micros)
        {
            return None;
        }
        Some(self)
    }
}

fn rounded_cost(parts: &[(u64, u64)]) -> Option<u64> {
    let numerator = parts.iter().try_fold(0_u128, |sum, (tokens, rate)| {
        sum.checked_add(u128::from(*tokens).checked_mul(u128::from(*rate))?)
    })?;
    let rounded = numerator.checked_add(TOKENS_PER_PRICE_UNIT - 1)? / TOKENS_PER_PRICE_UNIT;
    u64::try_from(rounded).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reservation_covers_cache_price_and_unknown_usage_keeps_upper_bound() {
        let prices = ApprovalTokenPrices {
            input_micros_per_million: 1_000_000,
            output_micros_per_million: 4_000_000,
            cache_read_micros_per_million: 100_000,
            cache_write_micros_per_million: 1_500_000,
        };
        assert_eq!(prices.reserve(1_000, 200), Some(2_300));
        assert_eq!(
            prices.actual(TokenUsage {
                input_tokens: Some(500),
                output_tokens: Some(100),
                cache_read_tokens: Some(250),
                cache_write_tokens: Some(250),
            }),
            Some(1_300)
        );
        assert_eq!(
            prices.actual(TokenUsage {
                input_tokens: None,
                ..Default::default()
            }),
            None
        );
        let (reserved_tokens, reserved_cost) =
            reviewer_reservation("review", prices, 32_768, 128_000).unwrap();
        assert!(reserved_tokens > 128_000);
        assert!(
            reserved_cost
                >= prices
                    .actual(TokenUsage {
                        input_tokens: Some(1_024),
                        output_tokens: Some(128),
                        ..Default::default()
                    })
                    .unwrap()
        );
        assert!(reviewer_reservation("review", prices, 100, 128_000).is_none());
    }
    #[test]
    fn runtime_output_budget_is_reserved_at_the_output_price() {
        let prices = ApprovalTokenPrices {
            input_micros_per_million: 1_000_000,
            output_micros_per_million: 2_000_000,
            cache_read_micros_per_million: 1_000_000,
            cache_write_micros_per_million: 1_000_000,
        };
        let (small_tokens, small_cost) =
            reviewer_reservation("review", prices, 32_768, 2048).unwrap();
        let (large_tokens, large_cost) =
            reviewer_reservation("review", prices, 32_768, 128_000).unwrap();
        assert_eq!(large_tokens - small_tokens, 128_000 - 2048);
        assert_eq!(large_cost - small_cost, 2 * (128_000 - 2048));
        assert!(reviewer_reservation("review", prices, 32_768, 0).is_none());
        assert!(reviewer_reservation("review", prices, 32_768, u64::MAX).is_none());
    }

    #[test]
    fn partial_provider_usage_holds_both_original_bounds() {
        for (tokens, cost) in [(None, None), (Some(17), None), (None, Some(9))] {
            let held =
                ReviewUsageSettlement::from_provider_fact(true, tokens, cost, 100, 200).unwrap();
            assert_eq!(held.state(), "unknown");
            assert_eq!((held.tokens, held.cost_micros), (100, 200));
            assert!(!held.usage_known);
        }
    }

    #[test]
    fn usage_receipt_accepts_actual_overage_but_rejects_fabricated_unstarted_usage() {
        let actual =
            ReviewUsageSettlement::from_provider_fact(true, Some(101), Some(201), 100, 200)
                .unwrap();
        assert_eq!(actual.state(), "known");
        assert_eq!((actual.tokens, actual.cost_micros), (101, 201));
        let unsent =
            ReviewUsageSettlement::from_provider_fact(false, None, Some(0), 100, 200).unwrap();
        assert_eq!(unsent.state(), "unstarted");
        assert_eq!((unsent.tokens, unsent.cost_micros), (0, 0));
        assert!(
            ReviewUsageSettlement::from_provider_fact(false, Some(1), None, 100, 200).is_none()
        );
        assert!(
            ReviewUsageSettlement::from_provider_fact(false, None, Some(1), 100, 200).is_none()
        );
    }

    #[test]
    fn unknown_receipt_is_bound_to_both_original_reserves_and_schema() {
        let unknown =
            ReviewUsageSettlement::from_provider_fact(true, None, None, 100, 200).unwrap();
        assert!(unknown.validate(101, 200).is_none());
        assert!(unknown.validate(100, 201).is_none());
        let mut invalid = unknown;
        invalid.schema_version = 2;
        assert!(invalid.validate(100, 200).is_none());
        invalid = unknown;
        invalid.provider_started = false;
        assert!(invalid.validate(100, 200).is_none());
        assert!(ReviewUsageSettlement::from_provider_fact(true, None, None, 0, 200).is_none());
    }
}
