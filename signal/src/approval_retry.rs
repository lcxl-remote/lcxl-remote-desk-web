//! A completed malformed answer may fund a new, independently fenced call.
use crate::agent_approval_store::ClaimedPermissionReview;
use crate::config::connection::{DatabaseConnection, DatabaseTransaction};
use crate::entity::{
    agent_approval_attempt as attempt, agent_approval_delegation as delegation,
    agent_approval_review as review,
};
use desk_diagnose_core::approval_review::{ApprovalReviewCandidate, ReviewFormatIssue};
use sea_orm::{ColumnTrait, DbErr, EntityTrait, QueryFilter, QuerySelect, Set};
fn invalid() -> DbErr {
    DbErr::Custom("approval correction is no longer available".into())
}

async fn current(
    txn: &DatabaseTransaction,
    claim: &ClaimedPermissionReview,
) -> Result<review::Model, DbErr> {
    let row = review::Entity::find()
        .filter(review::Column::CandidateId.eq(&claim.candidate_id))
        .lock_exclusive()
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    if row.status != "reviewing"
        || crate::approval_attempt::authority(&row)? != claim.call_authority
    {
        return Err(invalid());
    }
    Ok(row)
}
async fn save_delegation(
    txn: &DatabaseTransaction,
    row: &delegation::Model,
    value: &desk_diagnose_core::approval_delegation::ApprovalDelegation,
    now: i64,
) -> Result<(), DbErr> {
    let changed = delegation::Entity::update_many()
        .set(delegation::ActiveModel {
            state_json: Set(serde_json::to_string(value).map_err(|_| invalid())?),
            version: Set(i64::try_from(value.ledger_version).map_err(|_| invalid())?),
            updated_at: Set(now),
            ..Default::default()
        })
        .filter(delegation::Column::Id.eq(row.id))
        .filter(delegation::Column::Version.eq(row.version))
        .exec(txn)
        .await?;
    if changed.rows_affected != 1 {
        return Err(invalid());
    }
    Ok(())
}

/// Commit usage first. A later allocation failure cannot roll this contribution back.
async fn settle_completed(
    db: &DatabaseConnection,
    claim: &ClaimedPermissionReview,
    issue: ReviewFormatIssue,
    now: i64,
) -> Result<(), DbErr> {
    let txn = crate::db::begin_write(db, review::Entity).await?;
    let row = current(&txn, claim).await?;
    let old = attempt::Entity::find()
        .filter(
            attempt::Column::CallId.eq(claim.call_authority.attempt.call_id(&claim.candidate_id)),
        )
        .one(&txn)
        .await?
        .ok_or_else(invalid)?;
    if !old.response_complete
        || old.format_issue_json.as_deref()
            != Some(
                serde_json::to_string(&issue)
                    .map_err(|_| invalid())?
                    .as_str(),
            )
        || row.provider_receipt_id.is_none()
    {
        return Err(invalid());
    }
    if row.usage_settlement_json.is_some() {
        return Ok(());
    }
    let physical =
        crate::agent_subagent_store::settle_review_call_on(&txn, &row, None, now).await?;
    let accounted = physical.settlement(row.reserved_tokens)?;
    let stored = delegation::Entity::find()
        .filter(delegation::Column::DelegationId.eq(&row.delegation_id))
        .filter(delegation::Column::ActorId.eq(&row.actor_id))
        .filter(delegation::Column::DeviceId.eq(&row.device_id))
        .one(&txn)
        .await?
        .ok_or_else(invalid)?;
    let mut value = crate::agent_approval_store::decode(&stored)?;
    value
        .settle_tokens(row.reserved_tokens as u64, Some(accounted.tokens))
        .map_err(|_| invalid())?;

    save_delegation(&txn, &stored, &value, now).await?;
    crate::agent_approval_usage::record_usage_on(&txn, &row, accounted, now).await?;
    txn.commit().await?;
    Ok(())
}

pub(crate) async fn advance(
    db: &DatabaseConnection,
    candidate: &ApprovalReviewCandidate,
    claim: &ClaimedPermissionReview,
    issue: ReviewFormatIssue,
) -> Result<Option<ClaimedPermissionReview>, DbErr> {
    let Some(next) = claim.call_authority.attempt.next(issue) else {
        return Ok(None);
    };
    let now = chrono::Utc::now().timestamp_millis();
    settle_completed(db, claim, issue, now).await?;
    let txn = crate::db::begin_write(db, review::Entity).await?;
    let row = current(&txn, claim).await?;
    if now < 0
        || now as u64 >= claim.lease_deadline_unix_ms
        || now as u64 >= candidate.expires_at_unix_ms
        || row.usage_settlement_json.is_none()
    {
        return Err(invalid());
    }
    let config = crate::approval_model_provider::load(&txn).await?;
    if config.unavailable_reason().is_some()
        || u64::try_from(config.configuration_revision).ok() != Some(claim.model_config_revision)
        || config.destination_identity().ok().as_ref() != Some(&claim.model_destination)
    {
        return Err(invalid());
    }
    let output = u64::try_from(config.gateway.runtime_max_output_tokens).map_err(|_| invalid())?;
    let tokens = desk_diagnose_core::approval_usage::reviewer_token_reservation(
        &claim.authorized.prompt,
        u64::try_from(config.gateway.max_context_bytes.unwrap_or(131072)).map_err(|_| invalid())?,
        output,
    )
    .ok_or_else(invalid)?;

    let stored = delegation::Entity::find()
        .filter(delegation::Column::DelegationId.eq(&row.delegation_id))
        .filter(delegation::Column::ActorId.eq(&row.actor_id))
        .filter(delegation::Column::DeviceId.eq(&row.device_id))
        .lock_exclusive()
        .one(&txn)
        .await?
        .ok_or_else(invalid)?;
    let mut value = crate::agent_approval_store::decode(&stored)?;
    value.reserve_tokens(tokens).map_err(|_| invalid())?;
    save_delegation(&txn, &stored, &value, now).await?;
    let changed = review::Entity::update_many()
        .set(review::ActiveModel {
            call_authority_json: Set(None),
            delegation_reservation_id: Set(None),
            provider_receipt_kind: Set(None),
            provider_receipt_id: Set(None),
            provider_started_at_ms: Set(None),
            usage_settlement_json: Set(None),
            usage_settlement_state: Set(None),
            usage_reconcile_at_ms: Set(None),
            reserved_tokens: Set(i64::try_from(tokens).map_err(|_| invalid())?),
            updated_at: Set(now),
            ..Default::default()
        })
        .filter(review::Column::Id.eq(row.id))
        .filter(review::Column::CallAuthorityJson.eq(row.call_authority_json.clone()))
        .filter(review::Column::UsageSettlementJson.eq(row.usage_settlement_json.clone()))
        .exec(&txn)
        .await?;
    if changed.rows_affected != 1 {
        return Err(invalid());
    }
    let (authority, reservation) =
        Box::pin(crate::agent_subagent_store::reserve_review_attempt_on(
            &txn,
            candidate,
            &claim.model_destination,
            &claim.authorized,
            tokens,
            output,
            now,
            next,
        ))
        .await?;
    let mut expected = claim.call_authority.clone();
    expected.attempt = authority.attempt.clone();
    expected.request_sha256 = authority.request_sha256.clone();
    if authority != expected {
        return Err(invalid());
    }
    txn.commit().await?;
    Ok(Some(ClaimedPermissionReview {
        call_authority: authority,
        delegation_call: reservation,
        ..claim.clone()
    }))
}
