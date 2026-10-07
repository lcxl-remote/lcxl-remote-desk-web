//! Logical call reservations shared by model, compression, reviewer and tool paths.

use super::{budget::Usage, valid_id};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegationCallKind {
    Model,
    ContextSummary,
    ApprovalReview,
    SafetyReview,
    Tool,
}

impl DelegationCallKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::ContextSummary => "context_summary",
            Self::ApprovalReview => "approval_review",
            Self::SafetyReview => "safety_review",
            Self::Tool => "tool",
        }
    }
}

/// Exhaustion is an admission result; it never authorizes provider I/O.
#[derive(Debug, Clone, PartialEq, Eq)]
// Bounded records transfer as one owned value across admission and claim.
#[allow(clippy::large_enum_variant)]
pub enum CallAdmission {
    Untracked,
    Reserved(DelegationCallReservation),
    Exhausted,
}

pub fn known_model_usage(
    tokens: crate::chat::TokenUsage,
    elapsed_ms: u64,
) -> Option<crate::goal::GoalUsage> {
    let nonnegative =
        |value: Option<i64>| value.filter(|value| *value >= 0).map(|value| value as u64);
    let optional = |value: Option<i64>| match value {
        None => Some(0),
        Some(_) => nonnegative(value),
    };
    Some(crate::goal::GoalUsage {
        input_tokens: nonnegative(tokens.input_tokens)?,
        output_tokens: nonnegative(tokens.output_tokens)?,
        cache_read_tokens: optional(tokens.cache_read_tokens)?,
        cache_write_tokens: optional(tokens.cache_write_tokens)?,
        model_calls: 1,
        active_time_ms: elapsed_ms,
        ..crate::goal::GoalUsage::default()
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelegationCallReservation {
    pub reservation_id: String,
    pub logical_call_id: String,
    pub root_conversation_id: String,
    pub group_id: String,
    pub conversation_id: String,
    pub task_id: Option<String>,
    pub kind: DelegationCallKind,
    pub arguments_sha256: String,
    pub source_epoch: u64,
    pub input_revision: u64,
    pub control_revision: u64,
    pub planning_lease_token: Option<u64>,
    pub review_authority: Option<ReviewCallAuthority>,
    pub upper: Usage,
    /// Both constraints reserve one logical call. This is allocation accounting,
    /// not another provider call or billing event.
    pub source_goal_upper: Option<crate::goal::GoalUsage>,
}

impl DelegationCallReservation {
    /// A neutral admission is not permission to send a larger rendered request.
    /// Hosts compare the exact provider body and output limit before persisting
    /// their physical send fence; a mismatch leaves the call unsent.
    pub fn permits_rendered_tokens(&self, units: u64) -> bool {
        self.kind != DelegationCallKind::Tool && units > 0 && units <= self.upper.tokens
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if [
            &self.reservation_id,
            &self.logical_call_id,
            &self.root_conversation_id,
            &self.group_id,
            &self.conversation_id,
        ]
        .iter()
        .any(|id| !valid_id(id))
            || self.task_id.as_ref().is_some_and(|id| !valid_id(id))
            || self.arguments_sha256.len() != 64
            || !self
                .arguments_sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            || self.source_epoch == 0
            || self.source_epoch > i64::MAX as u64
            || self.input_revision == 0
            || self.input_revision > i64::MAX as u64
            || self.control_revision == 0
            || self.control_revision > i64::MAX as u64
            || self
                .planning_lease_token
                .is_some_and(|token| token == 0 || token > i64::MAX as u64)
            || self.planning_lease_token.is_some() == self.review_authority.is_some()
            || self.review_authority.as_ref().is_some_and(|review| {
                review.validate().is_err()
                    || self.kind != DelegationCallKind::ApprovalReview
                    || review.request_sha256 != self.arguments_sha256
                    || review.root_conversation_id != self.root_conversation_id
                    || review.group_id.as_ref() != Some(&self.group_id)
                    || review.conversation_id != self.conversation_id
                    || review.task_id != self.task_id
                    || review.input_revision != self.input_revision
                    || review.control_revision != self.control_revision
                    || review.source_epoch != Some(self.source_epoch)
            })
            || self.task_id.is_some() == (self.root_conversation_id == self.conversation_id)
            || match self.kind {
                DelegationCallKind::Tool => {
                    self.upper.tool_calls != 1
                        || self.upper.model_calls != 0
                        || self.upper.tokens != 0
                }
                _ => {
                    self.upper.model_calls != 1
                        || self.upper.tool_calls != 0
                        || self.upper.tokens == 0
                }
            }
            || self.source_goal_upper.is_some_and(|goal| {
                goal.slices != 0
                    || u64::from(goal.model_calls) != self.upper.model_calls
                    || u64::from(goal.tool_calls) != self.upper.tool_calls
                    || goal.total_tokens() != Some(self.upper.tokens)
            })
        {
            return Err("invalid delegation call reservation");
        }
        Ok(())
    }
}

/// Independent reviews own a separate durable lease. It grants one reviewer
/// call and never grants a child planner lease or a new task source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewCallAuthority {
    pub candidate_id: String,
    pub lease_epoch: u64,
    pub lease_owner: String,
    pub lease_deadline_ms: u64,
    pub model_config_revision: u64,
    pub delegation_id: String,
    pub delegation_revision: u64,
    pub policy_revision: u64,
    pub source: crate::approval_review::ApprovalSource,
    pub source_record_id: String,
    pub source_planning_lease_token: Option<u64>,
    pub context_hmac_sha256: String,
    pub request_sha256: String,
    pub model_destination: desk_agent_protocol::data_lineage::DestinationIdentity,
    pub root_conversation_id: String,
    pub group_id: Option<String>,
    pub task_id: Option<String>,
    pub conversation_id: String,
    pub actor_id: String,
    pub device_id: String,
    pub input_revision: u64,
    pub control_revision: u64,
    pub source_epoch: Option<u64>,
}

impl ReviewCallAuthority {
    pub fn validate(&self) -> Result<(), &'static str> {
        if [
            &self.candidate_id,
            &self.lease_owner,
            &self.root_conversation_id,
            &self.conversation_id,
            &self.actor_id,
            &self.device_id,
            &self.delegation_id,
            &self.source_record_id,
        ]
        .iter()
        .any(|id| !valid_id(id))
            || self.group_id.as_ref().is_some_and(|id| !valid_id(id))
            || self.task_id.as_ref().is_some_and(|id| !valid_id(id))
            || [
                self.lease_epoch,
                self.lease_deadline_ms,
                self.model_config_revision,
                self.input_revision,
                self.control_revision,
                self.delegation_revision,
                self.policy_revision,
            ]
            .iter()
            .any(|value| *value == 0 || *value > i64::MAX as u64)
            || self.context_hmac_sha256.len() != 64
            || !self
                .context_hmac_sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            || self.source.validate().is_err()
            || self.source_planning_lease_token.is_some()
                != matches!(
                    self.source,
                    crate::approval_review::ApprovalSource::ConcreteCall { .. }
                )
            || self
                .source_planning_lease_token
                .is_some_and(|token| token == 0 || token > i64::MAX as u64)
            || self.request_sha256.len() != 64
            || !self
                .request_sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            || self.model_destination.validate().is_err()
            || !matches!(
                self.model_destination,
                desk_agent_protocol::data_lineage::DestinationIdentity::Model { .. }
            )
            || self.group_id.is_some() != self.source_epoch.is_some()
            || self
                .source_epoch
                .is_some_and(|epoch| epoch == 0 || epoch > i64::MAX as u64)
            || self.task_id.is_some() == (self.root_conversation_id == self.conversation_id)
            || self.task_id.is_some() && self.group_id.is_none()
        {
            return Err("invalid independent review authority");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn authority() -> ReviewCallAuthority {
        ReviewCallAuthority {
            candidate_id: "candidate".into(),
            lease_epoch: 1,
            lease_owner: "review-worker".into(),
            lease_deadline_ms: 10000,
            model_config_revision: 1,
            delegation_id: "owner-delegation".into(),
            delegation_revision: 1,
            policy_revision: 1,
            source: crate::approval_review::ApprovalSource::PermissionItem {
                request_id: "request".into(),
                item_id: "item".into(),
            },
            source_record_id: "request:item".into(),
            source_planning_lease_token: None,
            context_hmac_sha256: "a".repeat(64),
            request_sha256: "b".repeat(64),
            model_destination: desk_agent_protocol::data_lineage::DestinationIdentity::Model {
                connection_id: "review-provider".into(),
                connection_revision: 1,
                model_id: "review-model".into(),
                profile_revision: 1,
            },
            root_conversation_id: "root".into(),
            group_id: Some("group".into()),
            task_id: Some("task".into()),
            conversation_id: "child".into(),
            actor_id: "1".into(),
            device_id: "1".into(),
            input_revision: 1,
            control_revision: 1,
            source_epoch: Some(1),
        }
    }
    #[test]
    fn rendered_request_cannot_exceed_the_original_token_reservation() {
        let mut reservation = receipt();
        assert!(reservation.permits_rendered_tokens(reservation.upper.tokens));
        assert!(!reservation.permits_rendered_tokens(reservation.upper.tokens + 1));
        assert!(!reservation.permits_rendered_tokens(0));
        reservation.kind = DelegationCallKind::Tool;
        assert!(!reservation.permits_rendered_tokens(1));
    }

    fn receipt() -> DelegationCallReservation {
        DelegationCallReservation {
            reservation_id: "reservation".into(),
            logical_call_id: "review:candidate".into(),
            root_conversation_id: "root".into(),
            group_id: "group".into(),
            conversation_id: "child".into(),
            task_id: Some("task".into()),
            kind: DelegationCallKind::ApprovalReview,
            arguments_sha256: "b".repeat(64),
            source_epoch: 1,
            input_revision: 1,
            control_revision: 1,
            planning_lease_token: None,
            review_authority: Some(authority()),
            upper: Usage {
                model_calls: 1,
                tokens: 5000,
                ..Default::default()
            },
            source_goal_upper: None,
        }
    }
    #[test]
    fn review_lease_is_disjoint_from_planning_and_never_authorizes_other_call_kinds() {
        let original = receipt();
        assert!(original.validate().is_ok());
        let mut both = original.clone();
        both.planning_lease_token = Some(1);
        assert!(both.validate().is_err());
        let mut concrete = original.clone();
        concrete.review_authority.as_mut().unwrap().source =
            crate::approval_review::ApprovalSource::ConcreteCall {
                call_id: "original-call".into(),
                grant_id: "original-grant".into(),
            };
        assert!(concrete.validate().is_err());
        concrete
            .review_authority
            .as_mut()
            .unwrap()
            .source_planning_lease_token = Some(4);
        assert!(concrete.validate().is_ok());
        assert!(concrete.planning_lease_token.is_none());
        let mut neither = original.clone();
        neither.review_authority = None;
        assert!(neither.validate().is_err());
        let mut planner = original.clone();
        planner.review_authority = None;
        planner.planning_lease_token = Some(1);
        assert!(planner.validate().is_ok());
        for kind in [
            DelegationCallKind::Model,
            DelegationCallKind::ContextSummary,
            DelegationCallKind::Tool,
        ] {
            let mut different = original.clone();
            different.kind = kind;
            assert!(different.validate().is_err());
        }
    }
    #[test]
    fn review_scope_source_request_and_model_destination_are_frozen() {
        let original = receipt();
        let mut other_input = original.clone();
        other_input.input_revision += 1;
        assert!(other_input.validate().is_err());
        let mut other_control = original.clone();
        other_control.control_revision += 1;
        assert!(other_control.validate().is_err());
        let mut other_source = original.clone();
        other_source.source_epoch += 1;
        assert!(other_source.validate().is_err());
        let mut other_body = original.clone();
        other_body.arguments_sha256 = "c".repeat(64);
        assert!(other_body.validate().is_err());
        let mut no_task_source = original.clone();
        no_task_source.review_authority.as_mut().unwrap().group_id = None;
        assert!(no_task_source.validate().is_err());
        let mut invalid_destination = original.clone();
        invalid_destination
            .review_authority
            .as_mut()
            .unwrap()
            .model_destination =
            desk_agent_protocol::data_lineage::DestinationIdentity::LocalArtifact {
                workspace_id: "workspace".into(),
            };
        assert!(invalid_destination.validate().is_err());
    }
    #[test]
    fn classifier_purpose_cannot_reuse_a_main_model_or_review_authority_allocation() {
        let mut reservation = receipt();
        reservation.review_authority = None;
        reservation.planning_lease_token = Some(7);
        reservation.kind = DelegationCallKind::SafetyReview;
        reservation.validate().unwrap();
        let mut request = crate::seam::ModelRequest::text_only(
            Vec::new(),
            crate::prompt::ResponseFormatSpec::JsonObject,
        );
        request.use_case = crate::model_profile::ModelUseCase::Safety;
        request.caller_output_hard_cap = Some(1);
        request.delegation_call = Some(reservation.clone());
        assert!(request.validate_delegation_call().is_ok());
        request.use_case = crate::model_profile::ModelUseCase::Agent;
        assert!(request.validate_delegation_call().is_err());
        request.use_case = crate::model_profile::ModelUseCase::Safety;
        request.delegation_call = Some(receipt());
        assert!(request.validate_delegation_call().is_err());
    }
}
