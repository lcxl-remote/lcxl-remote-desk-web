//! Intent-time gate for a concrete UI call under an AI-approved scope grant.

use crate::entity::agent_approval_review;
use desk_agent_protocol::capability_grant::{CapabilityGrant, CapabilityGrantIssuer};
use desk_diagnose_core::approval_review::{
    APPROVAL_REVIEW_SOURCE_CONCRETE_CALL, APPROVAL_REVIEW_STATUS_APPROVED, ApprovalReviewDecision,
    ApprovalVerdict, concrete_call_review_identity,
};
use sea_orm::{ColumnTrait, DatabaseTransaction, DbErr, EntityTrait, QueryFilter};

pub(super) async fn require_approved_on(
    txn: &DatabaseTransaction,
    grant: &CapabilityGrant,
    call_id: &str,
    canonical_input_json: &str,
    now_unix_ms: u64,
) -> Result<(), DbErr> {
    let identity = concrete_call_review_identity(grant, call_id, canonical_input_json)
        .map_err(|_| invalid())?;
    let Some(identity) = identity else {
        return Ok(());
    };
    let CapabilityGrantIssuer::AiApproval(parent) = &grant.issued_by else {
        return Err(invalid());
    };
    let review = agent_approval_review::Entity::find()
        .filter(agent_approval_review::Column::CandidateId.eq(&identity.candidate_id))
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    let decision: ApprovalReviewDecision = review
        .decision_json
        .as_deref()
        .ok_or_else(invalid)
        .and_then(|json| serde_json::from_str(json).map_err(|_| invalid()))?;
    if review.status != APPROVAL_REVIEW_STATUS_APPROVED
        || review.source_kind != APPROVAL_REVIEW_SOURCE_CONCRETE_CALL
        || review.source_id != identity.source_id
        || review.action_sha256 != identity.action_sha256
        || review.conversation_id != grant.run_id
        || review.actor_id != grant.actor_id
        || review.device_id != grant.target_device_id
        || review.delegation_id != parent.delegation_id
        || review.expires_at <= i64::try_from(now_unix_ms).map_err(|_| invalid())?
        || review.lease_owner.is_some()
        || review.lease_deadline.is_some()
        || decision.candidate_id != identity.candidate_id
        || decision.verdict != ApprovalVerdict::Approve
    {
        return Err(invalid());
    }
    Ok(())
}

fn invalid() -> DbErr {
    DbErr::Custom("AI-approved UI scope lacks a current concrete-call review".into())
}
