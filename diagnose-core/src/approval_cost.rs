//! Integer-only cost accounting for an independent approval-model call.
//! Prices are micro-units of the delegation's configured accounting currency
//! per one million tokens. Missing prices or usage cannot become a zero bill.

use serde::{Deserialize, Serialize};

use crate::chat::TokenUsage;

const TOKENS_PER_PRICE_UNIT: u128 = 1_000_000;
pub const APPROVAL_REVIEW_OUTPUT_TOKEN_RESERVE: u64 = 2_048;
const APPROVAL_REVIEW_FRAME_BYTE_RESERVE: u64 = 8 * 1_024;

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

    /// Reserve the full request context and the strict reviewer output cap.
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
/// byte bound for input tokens, wire framing, and the hard output cap. Oversized
/// requests hand off to a person before any provider dial.
pub fn reviewer_reservation(
    authorized_prompt: &str,
    prices: ApprovalTokenPrices,
    model_context_bytes: u64,
) -> Option<(u64, u64)> {
    let input_upper = u64::try_from(authorized_prompt.len())
        .ok()?
        .checked_add(
            u64::try_from(crate::approval_review::APPROVAL_REVIEW_SYSTEM_PROMPT.len()).ok()?,
        )?
        .checked_add(APPROVAL_REVIEW_FRAME_BYTE_RESERVE)?;
    if input_upper > model_context_bytes {
        return None;
    }
    let tokens = input_upper.checked_add(APPROVAL_REVIEW_OUTPUT_TOKEN_RESERVE)?;
    let cost = prices.reserve(input_upper, APPROVAL_REVIEW_OUTPUT_TOKEN_RESERVE)?;
    (cost > 0).then_some((tokens, cost))
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
            reviewer_reservation("review", prices, 32_768).unwrap();
        assert!(reserved_tokens > APPROVAL_REVIEW_OUTPUT_TOKEN_RESERVE);
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
        assert!(reviewer_reservation("review", prices, 100).is_none());
    }
}
