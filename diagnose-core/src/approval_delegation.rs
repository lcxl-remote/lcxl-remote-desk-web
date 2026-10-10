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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_used_micros: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_reserved_micros: Option<u64>,
}

impl Default for ApprovalDelegationUsage {
    fn default() -> Self {
        Self {
            reviews_used: 0,
            reviews_reserved: 0,
            tokens_used: 0,
            tokens_reserved: 0,
            cost_used_micros: Some(0),
            cost_reserved_micros: Some(0),
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
    /// Create an owner switch whose review usage has no monetary accounting.
    pub fn new_usage_only(
        delegation_id: String,
        conversation_id: String,
        owner_id: String,
        device_id: String,
        owner_authorization_id: String,
        created_at_unix_ms: u64,
    ) -> Result<Self, ApprovalDelegationError> {
        let mut value = Self::new(
            delegation_id,
            conversation_id,
            owner_id,
            device_id,
            owner_authorization_id,
            created_at_unix_ms,
        )?;
        value.usage.cost_used_micros = None;
        value.usage.cost_reserved_micros = None;
        Ok(value)
    }

    fn require_usage_only(&self) -> Result<(), ApprovalDelegationError> {
        self.validate()?;
        if self.usage.cost_used_micros.is_some() || self.usage.cost_reserved_micros.is_some() {
            return Err(ApprovalDelegationError::InvalidState);
        }
        Ok(())
    }

    pub fn reserve_tokens(&mut self, tokens: u64) -> Result<(), ApprovalDelegationError> {
        self.require_usage_only()?;
        if self.status != ApprovalDelegationStatus::Active || tokens == 0 {
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
            .checked_add(tokens)
            .ok_or(ApprovalDelegationError::BudgetExceeded)?;
        staged.ledger_version = staged
            .ledger_version
            .checked_add(1)
            .ok_or(ApprovalDelegationError::InvalidState)?;
        *self = staged;
        Ok(())
    }

    pub fn release_unstarted_review_tokens(
        &mut self,
        reserved_tokens: u64,
    ) -> Result<(), ApprovalDelegationError> {
        self.require_usage_only()?;
        if reserved_tokens == 0
            || self.usage.reviews_reserved == 0
            || self.usage.tokens_reserved < reserved_tokens
        {
            return Err(ApprovalDelegationError::InvalidState);
        }
        let version = self
            .ledger_version
            .checked_add(1)
            .ok_or(ApprovalDelegationError::InvalidState)?;
        self.usage.reviews_reserved -= 1;
        self.usage.tokens_reserved -= reserved_tokens;
        self.ledger_version = version;
        Ok(())
    }

    pub fn settle_tokens(
        &mut self,
        reserved_tokens: u64,
        actual_tokens: Option<u64>,
    ) -> Result<(), ApprovalDelegationError> {
        self.require_usage_only()?;
        let mut staged = self.clone();
        staged.release_unstarted_review_tokens(reserved_tokens)?;
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
        *self = staged;
        Ok(())
    }

    pub fn reconcile_review_token_usage(
        &mut self,
        previous: crate::approval_usage::ReviewTokenUsageSettlement,
        actual: crate::approval_usage::ReviewTokenUsageSettlement,
    ) -> Result<(), ApprovalDelegationError> {
        self.require_usage_only()?;
        if previous.schema_version != 1
            || actual.schema_version != 1
            || !previous.provider_started
            || previous.usage_known
            || !actual.provider_started
            || !actual.usage_known
            || previous.tokens == 0
            || self.usage.reviews_used == 0
        {
            return Err(ApprovalDelegationError::InvalidState);
        }
        let tokens = self
            .usage
            .tokens_used
            .checked_sub(previous.tokens)
            .and_then(|remaining| remaining.checked_add(actual.tokens))
            .ok_or(ApprovalDelegationError::InvalidState)?;
        let version = self
            .ledger_version
            .checked_add(1)
            .ok_or(ApprovalDelegationError::InvalidState)?;
        self.usage.tokens_used = tokens;
        self.ledger_version = version;
        Ok(())
    }

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
        if self.usage.cost_used_micros.is_some() != self.usage.cost_reserved_micros.is_some() {
            return Err(ApprovalDelegationError::InvalidState);
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
        staged.usage.cost_reserved_micros = Some(
            staged
                .usage
                .cost_reserved_micros
                .ok_or(ApprovalDelegationError::InvalidState)?
                .checked_add(max_cost_micros)
                .ok_or(ApprovalDelegationError::BudgetExceeded)?,
        );
        staged.ledger_version = staged
            .ledger_version
            .checked_add(1)
            .ok_or(ApprovalDelegationError::InvalidState)?;
        *self = staged;
        Ok(())
    }

    /// Release a claimed review whose atomic send boundary never committed.
    /// This changes accounting only and does not renew the delegation.
    pub fn release_unstarted_review(
        &mut self,
        reserved_tokens: u64,
        reserved_cost_micros: u64,
    ) -> Result<(), ApprovalDelegationError> {
        self.validate()?;
        if reserved_tokens == 0
            || reserved_cost_micros == 0
            || self.usage.reviews_reserved == 0
            || self.usage.tokens_reserved < reserved_tokens
            || self
                .usage
                .cost_reserved_micros
                .is_none_or(|cost| cost < reserved_cost_micros)
        {
            return Err(ApprovalDelegationError::BudgetExceeded);
        }
        let version = self
            .ledger_version
            .checked_add(1)
            .ok_or(ApprovalDelegationError::BudgetExceeded)?;
        self.usage.reviews_reserved -= 1;
        self.usage.tokens_reserved -= reserved_tokens;
        self.usage.cost_reserved_micros = self
            .usage
            .cost_reserved_micros
            .map(|cost| cost - reserved_cost_micros);
        self.ledger_version = version;
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
            || self
                .usage
                .cost_reserved_micros
                .is_none_or(|cost| cost < reserved_cost_micros)
        {
            return Err(ApprovalDelegationError::InvalidState);
        }
        let mut staged = self.clone();
        staged.usage.reviews_reserved -= 1;
        staged.usage.tokens_reserved -= reserved_tokens;
        staged.usage.cost_reserved_micros = staged
            .usage
            .cost_reserved_micros
            .map(|cost| cost - reserved_cost_micros);
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
        staged.usage.cost_used_micros = Some(
            staged
                .usage
                .cost_used_micros
                .ok_or(ApprovalDelegationError::InvalidState)?
                .checked_add(actual_cost_micros.unwrap_or(reserved_cost_micros))
                .ok_or(ApprovalDelegationError::InvalidState)?,
        );
        staged.ledger_version = staged
            .ledger_version
            .checked_add(1)
            .ok_or(ApprovalDelegationError::InvalidState)?;
        staged.validate()?;
        *self = staged;
        Ok(())
    }

    /// Replace one unknown review's held contribution with durable provider
    /// usage. The host CASes that record in the same transaction to prevent replay.
    pub fn reconcile_review_usage(
        &mut self,
        previous: crate::approval_cost::ReviewUsageSettlement,
        actual: crate::approval_cost::ReviewUsageSettlement,
    ) -> Result<(), ApprovalDelegationError> {
        self.validate()?;
        if previous.schema_version != 1
            || actual.schema_version != 1
            || !previous.provider_started
            || previous.usage_known
            || !actual.provider_started
            || !actual.usage_known
            || previous.tokens == 0
            || previous.cost_micros == 0
            || self.usage.reviews_used == 0
        {
            return Err(ApprovalDelegationError::InvalidState);
        }
        let mut staged = self.clone();
        staged.usage.tokens_used = staged
            .usage
            .tokens_used
            .checked_sub(previous.tokens)
            .and_then(|remaining| remaining.checked_add(actual.tokens))
            .ok_or(ApprovalDelegationError::InvalidState)?;
        staged.usage.cost_used_micros = Some(
            staged
                .usage
                .cost_used_micros
                .ok_or(ApprovalDelegationError::InvalidState)?
                .checked_sub(previous.cost_micros)
                .and_then(|remaining| remaining.checked_add(actual.cost_micros))
                .ok_or(ApprovalDelegationError::InvalidState)?,
        );
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

    fn usage_only() -> ApprovalDelegation {
        ApprovalDelegation::new_usage_only(
            "delegation".into(),
            "conversation".into(),
            "owner".into(),
            "device".into(),
            "owner-open-event".into(),
            1000,
        )
        .unwrap()
    }

    #[test]
    fn oss_usage_omits_money_and_reconciles_after_closing_without_renewing_authority() {
        use crate::approval_usage::ReviewTokenUsageSettlement;
        let mut value = usage_only();
        let encoded = serde_json::to_value(&value).unwrap();
        assert!(encoded["usage"].get("cost_used_micros").is_none());
        assert!(encoded["usage"].get("cost_reserved_micros").is_none());
        let decoded: ApprovalDelegation = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded, value);
        value.reserve_tokens(100).unwrap();
        value.settle_tokens(100, None).unwrap();
        value.close(ApprovalDelegationStatus::Closed).unwrap();
        let revision = value.revision;
        let previous = ReviewTokenUsageSettlement::from_provider_fact(true, None, 100).unwrap();
        let actual = ReviewTokenUsageSettlement::from_provider_fact(true, Some(7), 100).unwrap();
        value
            .reconcile_review_token_usage(previous, actual)
            .unwrap();
        assert_eq!(value.usage.tokens_used, 7);
        assert_eq!(value.usage.reviews_used, 1);
        assert_eq!(value.revision, revision);
        assert_eq!(value.status, ApprovalDelegationStatus::Closed);
        assert!(value.reserve_tokens(100).is_err());
    }

    #[test]
    fn oss_unsent_calls_release_tokens_and_modes_cannot_mix() {
        let mut value = usage_only();
        assert!(value.reserve(100, 100).is_err());
        value.reserve_tokens(100).unwrap();
        value.release_unstarted_review_tokens(100).unwrap();
        assert_eq!(value.usage.reviews_used, 0);
        assert_eq!(value.usage.tokens_used, 0);
        assert_eq!(value.usage.tokens_reserved, 0);
        let mut billed = delegation();
        assert!(billed.reserve_tokens(100).is_err());
        billed.reserve(100, 100).unwrap();
        assert!(billed.settle_tokens(100, Some(7)).is_err());
        billed.settle(100, 100, Some(7), Some(9)).unwrap();
        assert_eq!(billed.usage.cost_used_micros, Some(9));
    }

    #[test]
    fn token_only_usage_overflow_does_not_consume_a_reservation() {
        let mut value = usage_only();
        value.reserve_tokens(100).unwrap();
        value.usage.tokens_used = u64::MAX;
        let before = value.clone();
        assert!(value.settle_tokens(100, Some(1)).is_err());
        assert_eq!(value, before);
        value.usage.cost_used_micros = Some(0);
        assert!(value.validate().is_err());
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
        assert_eq!(delegation.usage.cost_used_micros, Some(20_000));
        assert_eq!(delegation.usage.reviews_reserved, 0);
    }

    #[test]
    fn unstarted_review_releases_allocation_without_a_used_review_or_new_authority() {
        let mut delegation = delegation();
        let revision = delegation.revision;
        delegation.reserve(4000, 20000).unwrap();
        delegation.close(ApprovalDelegationStatus::Closed).unwrap();
        let closed_revision = delegation.revision;
        delegation.release_unstarted_review(4000, 20000).unwrap();
        assert_eq!(delegation.usage, ApprovalDelegationUsage::default());
        assert_eq!(delegation.status, ApprovalDelegationStatus::Closed);
        assert_eq!(delegation.revision, closed_revision);
        assert!(closed_revision > revision);
        let before = delegation.clone();
        assert!(delegation.release_unstarted_review(4000, 20000).is_err());
        assert_eq!(delegation, before);
    }

    #[test]
    fn unstarted_release_overflow_does_not_partially_refund() {
        let mut delegation = delegation();
        delegation.reserve(4000, 20000).unwrap();
        delegation.ledger_version = u64::MAX;
        let before = delegation.clone();
        assert!(delegation.release_unstarted_review(4000, 20000).is_err());
        assert_eq!(delegation, before);
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
        delegation.usage.cost_used_micros = Some(10_000_000);
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
    #[test]
    fn late_review_accounting_replaces_one_contribution_without_reopening_authority() {
        use crate::approval_cost::ReviewUsageSettlement;
        let mut delegation = delegation();
        delegation.reserve(100, 200).unwrap();
        delegation.settle(100, 200, None, None).unwrap();
        delegation.reserve(20, 30).unwrap();
        delegation.settle(20, 30, Some(12), Some(13)).unwrap();
        delegation.reserve(40, 50).unwrap();
        delegation.close(ApprovalDelegationStatus::Closed).unwrap();
        let before = delegation.clone();
        let previous =
            ReviewUsageSettlement::from_provider_fact(true, None, None, 100, 200).unwrap();
        let actual =
            ReviewUsageSettlement::from_provider_fact(true, Some(7), Some(9), 100, 200).unwrap();
        delegation.reconcile_review_usage(previous, actual).unwrap();
        assert_eq!(delegation.usage.tokens_used, 19);
        assert_eq!(delegation.usage.cost_used_micros, Some(22));
        assert_eq!(delegation.usage.reviews_used, 2);
        assert_eq!(delegation.usage.reviews_reserved, 1);
        assert_eq!(delegation.usage.tokens_reserved, 40);
        assert_eq!(delegation.usage.cost_reserved_micros, Some(50));
        assert_eq!(delegation.status, before.status);
        assert_eq!(delegation.revision, before.revision);
        assert_eq!(delegation.ledger_version, before.ledger_version + 1);
        assert!(
            delegation
                .require_current("conversation", "owner", "device")
                .is_err()
        );
    }

    #[test]
    fn actual_review_overage_remains_fully_accounted() {
        use crate::approval_cost::ReviewUsageSettlement;
        let mut delegation = delegation();
        delegation.reserve(100, 200).unwrap();
        delegation.settle(100, 200, Some(150), Some(250)).unwrap();
        delegation.reserve(100, 200).unwrap();
        delegation.settle(100, 200, None, None).unwrap();
        let previous =
            ReviewUsageSettlement::from_provider_fact(true, None, None, 100, 200).unwrap();
        let actual =
            ReviewUsageSettlement::from_provider_fact(true, Some(170), Some(290), 100, 200)
                .unwrap();
        delegation.reconcile_review_usage(previous, actual).unwrap();
        assert_eq!(delegation.usage.tokens_used, 320);
        assert_eq!(delegation.usage.cost_used_micros, Some(540));
        assert_eq!(delegation.usage.reviews_used, 2);
    }

    #[test]
    fn late_review_invalid_transition_and_overflow_preserve_the_entire_ledger() {
        use crate::approval_cost::ReviewUsageSettlement;
        let mut delegation = delegation();
        delegation.reserve(100, 200).unwrap();
        delegation.settle(100, 200, None, None).unwrap();
        let unknown =
            ReviewUsageSettlement::from_provider_fact(true, None, None, 100, 200).unwrap();
        let known =
            ReviewUsageSettlement::from_provider_fact(true, Some(7), Some(9), 100, 200).unwrap();
        let unstarted =
            ReviewUsageSettlement::from_provider_fact(false, None, None, 100, 200).unwrap();
        for (previous, actual) in [
            (known, known),
            (unknown, unknown),
            (unstarted, known),
            (unknown, unstarted),
        ] {
            let before = delegation.clone();
            assert!(delegation.reconcile_review_usage(previous, actual).is_err());
            assert_eq!(delegation, before);
        }
        delegation.usage.tokens_used = 99;
        let before = delegation.clone();
        assert!(delegation.reconcile_review_usage(unknown, known).is_err());
        assert_eq!(delegation, before);
        delegation.usage.tokens_used = u64::MAX;
        let huge =
            ReviewUsageSettlement::from_provider_fact(true, Some(101), Some(9), 100, 200).unwrap();
        let before = delegation.clone();
        assert!(delegation.reconcile_review_usage(unknown, huge).is_err());
        assert_eq!(delegation, before);
        delegation.usage.tokens_used = 100;
        delegation.ledger_version = u64::MAX;
        let before = delegation.clone();
        assert!(delegation.reconcile_review_usage(unknown, known).is_err());
        assert_eq!(delegation, before);
    }
}
