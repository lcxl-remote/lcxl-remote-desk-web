//! Root/group limits with charged reservations and a parent synthesis reserve.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Usage {
    pub model_calls: u64,
    pub tool_calls: u64,
    pub tokens: u64,
}

impl Usage {
    pub fn checked_add(self, other: Self) -> Result<Self, &'static str> {
        Ok(Self {
            model_calls: self
                .model_calls
                .checked_add(other.model_calls)
                .ok_or("model call counter exhausted")?,
            tool_calls: self
                .tool_calls
                .checked_add(other.tool_calls)
                .ok_or("tool call counter exhausted")?,
            tokens: self
                .tokens
                .checked_add(other.tokens)
                .ok_or("token counter exhausted")?,
        })
    }

    pub fn fits_allowance(self, ceiling: Allowance) -> bool {
        ceiling.permits(self)
    }

    pub fn fits(self, ceiling: Self) -> bool {
        self.model_calls <= ceiling.model_calls
            && self.tool_calls <= ceiling.tool_calls
            && self.tokens <= ceiling.tokens
    }
}

/// Optional cumulative token limit; usage counters remain finite integers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Allowance {
    pub model_calls: u64,
    pub tool_calls: u64,
    pub tokens: Option<u64>,
}

impl Allowance {
    pub fn permits(self, usage: Usage) -> bool {
        usage.model_calls <= self.model_calls
            && usage.tool_calls <= self.tool_calls
            && self.tokens.is_none_or(|limit| usage.tokens <= limit)
    }

    pub fn has_model_capacity(self, usage: Usage) -> bool {
        usage.model_calls < self.model_calls && self.tokens.is_none_or(|limit| usage.tokens < limit)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelegationLimits {
    pub total: Allowance,
    pub max_context_bytes: u64,
    pub max_result_bytes: u64,
    pub deadline_ms: i64,
}

impl DelegationLimits {
    pub fn validate(self) -> Result<(), &'static str> {
        if self.total.model_calls == 0
            || self.total.tool_calls == 0
            || self.total.tokens == Some(0)
            || self.max_context_bytes < crate::MIN_MODEL_CONTEXT_BYTES as u64
            || self.max_context_bytes > crate::MAX_MODEL_CONTEXT_BYTES as u64
            || self.max_result_bytes == 0
            || self.max_result_bytes
                > desk_agent_protocol::ai_assistant::subagent::MAX_SUBAGENT_REPORT_BYTES as u64
            || self.deadline_ms <= 0
        {
            return Err("invalid delegation limits");
        }
        Ok(())
    }

    pub fn child_ceiling(self) -> Allowance {
        fn reserve(value: u64) -> u64 {
            value / 5 + u64::from(!value.is_multiple_of(5))
        }
        Allowance {
            model_calls: self.total.model_calls - reserve(self.total.model_calls),
            tool_calls: self.total.tool_calls,
            tokens: self.total.tokens.map(|limit| limit - reserve(limit)),
        }
    }
}

/// Both actual usage and unresolved reservations count against admission.
/// Unknown provider usage must remain reserved until explicitly reconciled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetLedger {
    pub charged: Usage,
    pub outstanding: Usage,
    pub child_charged: Usage,
    pub child_outstanding: Usage,
}

impl BudgetLedger {
    pub fn reserve(
        &mut self,
        limits: DelegationLimits,
        amount: Usage,
        child: bool,
        now_ms: i64,
    ) -> Result<(), &'static str> {
        limits.validate()?;
        if now_ms >= limits.deadline_ms {
            return Err("delegation deadline reached");
        }
        let outstanding = self.outstanding.checked_add(amount)?;
        if !limits.total.permits(self.charged.checked_add(outstanding)?) {
            return Err("delegation budget exhausted");
        }
        let child_outstanding = if child {
            self.child_outstanding.checked_add(amount)?
        } else {
            self.child_outstanding
        };
        if !limits
            .child_ceiling()
            .permits(self.child_charged.checked_add(child_outstanding)?)
        {
            return Err("delegation child budget exhausted; parent synthesis capacity is reserved");
        }
        self.outstanding = outstanding;
        self.child_outstanding = child_outstanding;
        Ok(())
    }

    /// The store locks the reservation and ledger together and settles each call once.
    pub fn settle(
        &mut self,
        reserved: Usage,
        actual: Usage,
        child: bool,
    ) -> Result<(), &'static str> {
        fn subtract(left: Usage, right: Usage) -> Result<Usage, &'static str> {
            Ok(Usage {
                model_calls: left
                    .model_calls
                    .checked_sub(right.model_calls)
                    .ok_or("missing model reservation")?,
                tool_calls: left
                    .tool_calls
                    .checked_sub(right.tool_calls)
                    .ok_or("missing tool reservation")?,
                tokens: left
                    .tokens
                    .checked_sub(right.tokens)
                    .ok_or("missing token reservation")?,
            })
        }
        let outstanding = subtract(self.outstanding, reserved)?;
        let charged = self.charged.checked_add(actual)?;
        let (child_outstanding, child_charged) = if child {
            (
                subtract(self.child_outstanding, reserved)?,
                self.child_charged.checked_add(actual)?,
            )
        } else {
            (self.child_outstanding, self.child_charged)
        };
        self.outstanding = outstanding;
        self.charged = charged;
        self.child_outstanding = child_outstanding;
        self.child_charged = child_charged;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn limits(tokens: Option<u64>, calls: u64) -> DelegationLimits {
        DelegationLimits {
            total: Allowance {
                model_calls: calls,
                tool_calls: 20,
                tokens,
            },
            max_context_bytes: 4096,
            max_result_bytes: 8192,
            deadline_ms: 10_000,
        }
    }
    #[test]
    fn unlimited_tokens_keep_counts_deadlines_and_checked_usage() {
        let mut ledger = BudgetLedger::default();
        let limits = limits(None, 10);
        assert_eq!(limits.child_ceiling().tokens, None);
        let upper = Usage {
            model_calls: 1,
            tokens: 2_000_000,
            ..Default::default()
        };
        ledger.reserve(limits, upper, true, 1).unwrap();
        ledger.settle(upper, upper, true).unwrap();
        ledger.reserve(limits, upper, true, 2).unwrap();
        assert!(ledger.reserve(limits, upper, true, 10_000).is_err());
        assert!(!limits.total.has_model_capacity(Usage {
            model_calls: 10,
            ..Default::default()
        }));
        assert!(
            !Usage {
                tokens: 2,
                ..Default::default()
            }
            .fits(Usage {
                tokens: 1,
                ..Default::default()
            })
        );
        assert!(
            Usage {
                tokens: u64::MAX,
                ..Default::default()
            }
            .checked_add(upper)
            .is_err()
        );
    }
    #[test]
    fn finite_derived_zero_is_exhausted_and_parent_reserve_is_preserved() {
        let tiny = limits(Some(1), 1);
        assert_eq!(tiny.child_ceiling().tokens, Some(0));
        assert_eq!(tiny.child_ceiling().model_calls, 0);
        assert!(!tiny.child_ceiling().has_model_capacity(Usage::default()));
        assert!(limits(Some(0), 10).validate().is_err());
        let bounded = limits(Some(100), 10);
        let mut ledger = BudgetLedger::default();
        let child = Usage {
            model_calls: 1,
            tokens: 80,
            ..Default::default()
        };
        ledger.reserve(bounded, child, true, 1).unwrap();
        assert!(
            ledger
                .reserve(
                    bounded,
                    Usage {
                        tokens: 1,
                        ..Default::default()
                    },
                    true,
                    1
                )
                .is_err()
        );
        ledger
            .reserve(
                bounded,
                Usage {
                    model_calls: 1,
                    tokens: 20,
                    ..Default::default()
                },
                false,
                1,
            )
            .unwrap();
        assert!(
            ledger
                .reserve(
                    bounded,
                    Usage {
                        tokens: 1,
                        ..Default::default()
                    },
                    false,
                    1
                )
                .is_err()
        );
    }
}
