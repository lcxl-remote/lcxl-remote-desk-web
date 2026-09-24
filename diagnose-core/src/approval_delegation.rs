//! Owner-scoped switch to ask an independent model to review
//! pending AI Assistant actions. A delegation is never a capability grant.

use crate::dynamic_run::{AgentRunEvent, AgentRunEventKind};
use serde::{Deserialize, Serialize};

pub const APPROVAL_DELEGATION_SCHEMA_VERSION: u16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDelegationStatus {
    Active,
    Closed,
    Invalidated,
}

impl ApprovalDelegationStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Closed => "closed",
            Self::Invalidated => "invalidated",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalDelegationUsage {
    pub reviews_used: u64,
    pub reviews_reserved: u32,
    pub tokens_used: u64,
    pub tokens_reserved: u64,
    pub cost_used_micros: u64,
    pub cost_reserved_micros: u64,
}

impl Default for ApprovalDelegationUsage {
    fn default() -> Self {
        Self {
            reviews_used: 0,
            reviews_reserved: 0,
            tokens_used: 0,
            tokens_reserved: 0,
            cost_used_micros: 0,
            cost_reserved_micros: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalDelegation {
    pub schema_version: u16,
    pub delegation_id: String,
    pub conversation_id: String,
    pub owner_id: String,
    pub device_id: String,
    /// Owner authority revision. Usage accounting must not invalidate an
    /// already issued grant bound to this delegation.
    pub revision: u64,
    /// Optimistic concurrency version for usage reservation and settlement.
    pub ledger_version: u64,
    pub status: ApprovalDelegationStatus,
    pub usage: ApprovalDelegationUsage,
    /// Stable owner-issued operation id; the store writes the matching audit event.
    pub owner_authorization_id: String,
    pub created_at_unix_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalDelegationError {
    UnsupportedVersion,
    InvalidIdentity,
    InvalidState,
    BudgetExceeded,
    StaleAuthority,
}

/// The ledger records who enabled or disabled the delegated reviewer without
/// duplicating model credentials, restrictions or conversation content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalDelegationAuditEvent {
    pub event: AgentRunEvent,
    pub delegation_id: String,
    pub owner_decision_id: String,
    pub delegation_revision: u64,
    pub status: ApprovalDelegationStatus,
}

impl ApprovalDelegationAuditEvent {
    pub fn validate_for(
        &self,
        delegation: &ApprovalDelegation,
    ) -> Result<(), ApprovalDelegationError> {
        self.event
            .validate()
            .map_err(|_| ApprovalDelegationError::InvalidState)?;
        if !matches!(
            (self.event.kind, self.status),
            (
                AgentRunEventKind::ApprovalDelegationOpened,
                ApprovalDelegationStatus::Active
            ) | (
                AgentRunEventKind::ApprovalDelegationClosed,
                ApprovalDelegationStatus::Closed
            )
        ) || self.event.run_id != delegation.conversation_id
            || self.event.correlation_id.as_deref() != Some(delegation.delegation_id.as_str())
            || self.delegation_id != delegation.delegation_id
            || valid_id(&self.owner_decision_id).is_err()
            || (self.status == ApprovalDelegationStatus::Active
                && self.owner_decision_id != delegation.owner_authorization_id)
            || self.delegation_revision != delegation.revision
            || self.status != delegation.status
        {
            return Err(ApprovalDelegationError::InvalidState);
        }
        Ok(())
    }
}

impl ApprovalDelegation {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        delegation_id: String,
        conversation_id: String,
        owner_id: String,
        device_id: String,
        owner_authorization_id: String,
        created_at_unix_ms: u64,
    ) -> Result<Self, ApprovalDelegationError> {
        let delegation = Self {
            schema_version: APPROVAL_DELEGATION_SCHEMA_VERSION,
            delegation_id,
            conversation_id,
            owner_id,
            device_id,
            revision: 1,
            ledger_version: 1,
            status: ApprovalDelegationStatus::Active,
            usage: ApprovalDelegationUsage::default(),
            owner_authorization_id,
            created_at_unix_ms,
        };
        delegation.validate()?;
        Ok(delegation)
    }

    pub fn validate(&self) -> Result<(), ApprovalDelegationError> {
        if self.schema_version != APPROVAL_DELEGATION_SCHEMA_VERSION {
            return Err(ApprovalDelegationError::UnsupportedVersion);
        }
        for identity in [
            &self.delegation_id,
            &self.conversation_id,
            &self.owner_id,
            &self.device_id,
            &self.owner_authorization_id,
        ] {
            valid_id(identity)?;
        }
        if self.revision == 0 || self.ledger_version < self.revision || self.created_at_unix_ms == 0
        {
            return Err(ApprovalDelegationError::InvalidIdentity);
        }
        Ok(())
    }

    pub fn require_current(
        &self,
        conversation_id: &str,
        owner_id: &str,
        device_id: &str,
    ) -> Result<(), ApprovalDelegationError> {
        self.validate()?;
        if self.status != ApprovalDelegationStatus::Active {
            return Err(ApprovalDelegationError::InvalidState);
        }
        if self.conversation_id != conversation_id
            || self.owner_id != owner_id
            || self.device_id != device_id
        {
            return Err(ApprovalDelegationError::StaleAuthority);
        }
        Ok(())
    }

    /// Reserve before the model dial, in the same database transaction that
    /// claims the review candidate. The caller provides candidate idempotency.
    pub fn reserve(
        &mut self,
        max_tokens: u64,
        max_cost_micros: u64,
    ) -> Result<(), ApprovalDelegationError> {
        self.validate()?;
        if self.status != ApprovalDelegationStatus::Active
            || max_tokens == 0
            || max_cost_micros == 0
        {
            return Err(ApprovalDelegationError::InvalidState);
        }
        let mut staged = self.clone();
        staged.usage.reviews_reserved = staged
            .usage
            .reviews_reserved
            .checked_add(1)
            .ok_or(ApprovalDelegationError::BudgetExceeded)?;
        staged.usage.tokens_reserved = staged
            .usage
            .tokens_reserved
            .checked_add(max_tokens)
            .ok_or(ApprovalDelegationError::BudgetExceeded)?;
        staged.usage.cost_reserved_micros = staged
            .usage
            .cost_reserved_micros
            .checked_add(max_cost_micros)
            .ok_or(ApprovalDelegationError::BudgetExceeded)?;
        staged.ledger_version = staged
            .ledger_version
            .checked_add(1)
            .ok_or(ApprovalDelegationError::InvalidState)?;
        *self = staged;
        Ok(())
    }

    /// Unknown usage is charged at the reserved upper bound. No failure path
    /// silently refunds a request whose provider may already have processed it.
    pub fn settle(
        &mut self,
        reserved_tokens: u64,
        reserved_cost_micros: u64,
        actual_tokens: Option<u64>,
        actual_cost_micros: Option<u64>,
    ) -> Result<(), ApprovalDelegationError> {
        self.validate()?;
        if self.usage.reviews_reserved == 0
            || self.usage.tokens_reserved < reserved_tokens
            || self.usage.cost_reserved_micros < reserved_cost_micros
            || actual_tokens.is_some_and(|used| used > reserved_tokens)
            || actual_cost_micros.is_some_and(|used| used > reserved_cost_micros)
        {
            return Err(ApprovalDelegationError::InvalidState);
        }
        let mut staged = self.clone();
        staged.usage.reviews_reserved -= 1;
        staged.usage.tokens_reserved -= reserved_tokens;
        staged.usage.cost_reserved_micros -= reserved_cost_micros;
        staged.usage.reviews_used = staged
            .usage
            .reviews_used
            .checked_add(1)
            .ok_or(ApprovalDelegationError::InvalidState)?;
        staged.usage.tokens_used = staged
            .usage
            .tokens_used
            .checked_add(actual_tokens.unwrap_or(reserved_tokens))
            .ok_or(ApprovalDelegationError::InvalidState)?;
        staged.usage.cost_used_micros = staged
            .usage
            .cost_used_micros
            .checked_add(actual_cost_micros.unwrap_or(reserved_cost_micros))
            .ok_or(ApprovalDelegationError::InvalidState)?;
        staged.ledger_version = staged
            .ledger_version
            .checked_add(1)
            .ok_or(ApprovalDelegationError::InvalidState)?;
        staged.validate()?;
        *self = staged;
        Ok(())
    }

    pub fn close(
        &mut self,
        status: ApprovalDelegationStatus,
    ) -> Result<(), ApprovalDelegationError> {
        self.validate()?;
        if self.status != ApprovalDelegationStatus::Active
            || status == ApprovalDelegationStatus::Active
        {
            return Err(ApprovalDelegationError::InvalidState);
        }
        let next_revision = self
            .revision
            .checked_add(1)
            .ok_or(ApprovalDelegationError::InvalidState)?;
        let next_ledger_version = self
            .ledger_version
            .checked_add(1)
            .ok_or(ApprovalDelegationError::InvalidState)?;
        self.status = status;
        self.revision = next_revision;
        self.ledger_version = next_ledger_version;
        Ok(())
    }
}

fn valid_id(value: &str) -> Result<(), ApprovalDelegationError> {
    if value.is_empty()
        || value.len() > 256
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        Err(ApprovalDelegationError::InvalidIdentity)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delegation() -> ApprovalDelegation {
        ApprovalDelegation::new(
            "delegation".into(),
            "conversation".into(),
            "owner".into(),
            "device".into(),
            "owner-open-event".into(),
            1_000,
        )
        .unwrap()
    }

    #[test]
    fn switch_has_no_goal_model_or_expiry_binding() {
        let delegation = delegation();
        assert!(
            delegation
                .require_current("conversation", "owner", "device")
                .is_ok()
        );
        let stored = serde_json::to_value(&delegation).unwrap();
        for key in [
            "goal_id",
            "goal_revision",
            "model_destination",
            "model_config_revision",
            "policy_revision",
            "additional_prohibitions",
            "expires_at_unix_ms",
            "limits",
        ] {
            assert!(
                stored.get(key).is_none(),
                "unexpected switch binding: {key}"
            );
        }
        assert_eq!(
            delegation.require_current("conversation", "other-owner", "device",),
            Err(ApprovalDelegationError::StaleAuthority)
        );
    }

    #[test]
    fn unknown_usage_charges_the_reserved_upper_bound() {
        let mut delegation = delegation();
        let authority_revision = delegation.revision;
        delegation.reserve(4_000, 20_000).unwrap();
        delegation.settle(4_000, 20_000, None, None).unwrap();
        assert_eq!(delegation.revision, authority_revision);
        assert_eq!(delegation.ledger_version, 3);
        assert_eq!(delegation.usage.reviews_used, 1);
        assert_eq!(delegation.usage.tokens_used, 4_000);
        assert_eq!(delegation.usage.cost_used_micros, 20_000);
        assert_eq!(delegation.usage.reviews_reserved, 0);
    }

    #[test]
    fn closing_changes_authority_and_ledger_versions() {
        let mut delegation = delegation();
        delegation.reserve(4_000, 20_000).unwrap();
        delegation
            .settle(4_000, 20_000, Some(20), Some(10))
            .unwrap();
        delegation.close(ApprovalDelegationStatus::Closed).unwrap();
        assert_eq!(delegation.revision, 2);
        assert_eq!(delegation.ledger_version, 4);
    }

    #[test]
    fn switch_does_not_expire_after_cumulative_review_usage() {
        let mut delegation = delegation();
        delegation.usage.reviews_used = 1_000_000;
        delegation.usage.tokens_used = 10_000_000;
        delegation.usage.cost_used_micros = 10_000_000;
        delegation.reserve(128_001, 1).unwrap();
        delegation.settle(128_001, 1, Some(1), Some(1)).unwrap();
        assert_eq!(delegation.usage.reviews_used, 1_000_001);
        assert!(
            delegation
                .require_current("conversation", "owner", "device")
                .is_ok()
        );
    }

    #[test]
    fn rejected_accounting_overflow_preserves_counters() {
        let mut delegation = delegation();
        delegation.usage.tokens_reserved = u64::MAX;
        let before = delegation.clone();
        assert_eq!(
            delegation.reserve(1, 1),
            Err(ApprovalDelegationError::BudgetExceeded)
        );
        assert_eq!(delegation, before);
    }
}
