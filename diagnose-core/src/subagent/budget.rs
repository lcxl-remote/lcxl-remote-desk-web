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

    pub fn fits(self, ceiling: Self) -> bool {
        self.model_calls <= ceiling.model_calls
            && self.tool_calls <= ceiling.tool_calls
            && self.tokens <= ceiling.tokens
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelegationLimits {
    pub total: Usage,
    pub max_context_bytes: u64,
    pub max_result_bytes: u64,
    pub deadline_ms: i64,
}

impl DelegationLimits {
    pub fn validate(self) -> Result<(), &'static str> {
        if self.total.model_calls == 0
            || self.total.tool_calls == 0
            || self.total.tokens == 0
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

    pub fn child_ceiling(self) -> Usage {
        fn reserve(value: u64) -> u64 {
            value / 5 + u64::from(!value.is_multiple_of(5))
        }
        Usage {
            model_calls: self.total.model_calls - reserve(self.total.model_calls),
            tool_calls: self.total.tool_calls,
            tokens: self.total.tokens - reserve(self.total.tokens),
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
        if !self.charged.checked_add(outstanding)?.fits(limits.total) {
            return Err("delegation budget exhausted");
        }
        let child_outstanding = if child {
            self.child_outstanding.checked_add(amount)?
        } else {
            self.child_outstanding
        };
        if !self
            .child_charged
            .checked_add(child_outstanding)?
            .fits(limits.child_ceiling())
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
