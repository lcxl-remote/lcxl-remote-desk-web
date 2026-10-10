//! Token-only reservations and late usage for independent review calls.

use serde::{Deserialize, Serialize};

const APPROVAL_REVIEW_FRAME_BYTE_RESERVE: u64 = 8 * 1_024;

/// Bound the complete reviewer request before dialing a provider.
pub fn reviewer_token_reservation(
    prompt: &str,
    model_context_bytes: u64,
    runtime_max_output_tokens: u64,
) -> Option<u64> {
    if runtime_max_output_tokens == 0 {
        return None;
    }
    let input_upper = u64::try_from(prompt.len())
        .ok()?
        .checked_add(
            u64::try_from(crate::approval_review::APPROVAL_REVIEW_SYSTEM_PROMPT.len()).ok()?,
        )?
        .checked_add(APPROVAL_REVIEW_FRAME_BYTE_RESERVE)?;
    if input_upper > model_context_bytes {
        return None;
    }
    input_upper.checked_add(runtime_max_output_tokens)
}

/// Split the same review reservation across a source goal's token counters.
pub fn reviewer_goal_usage(
    reserved_tokens: u64,
    runtime_max_output_tokens: u64,
    active_time_ms: u64,
) -> Option<crate::goal::GoalUsage> {
    if runtime_max_output_tokens == 0 {
        return None;
    }
    Some(crate::goal::GoalUsage {
        input_tokens: reserved_tokens.checked_sub(runtime_max_output_tokens)?,
        output_tokens: runtime_max_output_tokens,
        model_calls: 1,
        active_time_ms,
        ..Default::default()
    })
}

/// One review's contribution to usage counters, independent of billing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewTokenUsageSettlement {
    pub schema_version: u16,
    pub provider_started: bool,
    pub usage_known: bool,
    pub tokens: u64,
}

impl ReviewTokenUsageSettlement {
    pub fn from_provider_fact(
        provider_started: bool,
        tokens: Option<u64>,
        reserved_tokens: u64,
    ) -> Option<Self> {
        if reserved_tokens == 0 || (!provider_started && tokens.is_some_and(|tokens| tokens != 0)) {
            return None;
        }
        let value = Self {
            schema_version: 1,
            provider_started,
            usage_known: !provider_started || tokens.is_some(),
            tokens: if provider_started {
                tokens.unwrap_or(reserved_tokens)
            } else {
                0
            },
        };
        value.validate(reserved_tokens)
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

    pub fn validate(self, reserved_tokens: u64) -> Option<Self> {
        if self.schema_version != 1
            || reserved_tokens == 0
            || (!self.provider_started && (!self.usage_known || self.tokens != 0))
            || (self.provider_started && !self.usage_known && self.tokens != reserved_tokens)
        {
            return None;
        }
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reservations_bound_the_context_without_prices() {
        let reserved = reviewer_token_reservation("review", 32768, 128_000).unwrap();
        assert!(reserved > 128_000);
        assert!(reviewer_token_reservation("review", 100, 128_000).is_none());
    }

    #[test]
    fn reservations_follow_the_runtime_output_budget_and_reject_invalid_bounds() {
        let small = reviewer_token_reservation("review", 32768, 2048).unwrap();
        let large = reviewer_token_reservation("review", 32768, 128_000).unwrap();
        assert_eq!(large - small, 128_000 - 2048);
        assert!(reviewer_token_reservation("review", 32768, 0).is_none());
        assert!(reviewer_token_reservation("review", 32768, u64::MAX).is_none());
    }

    #[test]
    fn goal_usage_keeps_the_configured_input_and_output_reservations() {
        let tokens = reviewer_token_reservation("review", 32768, 128_000).unwrap();
        let usage = reviewer_goal_usage(tokens, 128_000, 240_000).unwrap();
        assert_eq!(usage.output_tokens, 128_000);
        assert_eq!(usage.input_tokens, tokens - 128_000);
        assert_eq!(usage.total_tokens(), Some(tokens));
        assert_eq!(usage.model_calls, 1);
        assert_eq!(usage.active_time_ms, 240_000);
        assert!(reviewer_goal_usage(tokens, 0, 1).is_none());
        assert!(reviewer_goal_usage(127_999, 128_000, 1).is_none());
    }

    #[test]
    fn unknown_usage_is_held_and_unsent_calls_use_no_tokens() {
        let held = ReviewTokenUsageSettlement::from_provider_fact(true, None, 100).unwrap();
        assert_eq!(held.state(), "unknown");
        assert_eq!(held.tokens, 100);
        let unsent = ReviewTokenUsageSettlement::from_provider_fact(false, None, 100).unwrap();
        assert_eq!(unsent.state(), "unstarted");
        assert_eq!(unsent.tokens, 0);
        assert!(ReviewTokenUsageSettlement::from_provider_fact(false, Some(1), 100).is_none());
        assert!(ReviewTokenUsageSettlement::from_provider_fact(true, None, 0).is_none());
        assert!(
            ReviewTokenUsageSettlement { tokens: 99, ..held }
                .validate(100)
                .is_none()
        );
        let actual = ReviewTokenUsageSettlement::from_provider_fact(true, Some(120), 100).unwrap();
        assert_eq!(actual.state(), "known");
        assert_eq!(actual.tokens, 120);
    }
}
