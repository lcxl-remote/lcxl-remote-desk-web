//! Durable physical-call facts. Optional trace content is never an authority source.
use crate::config::connection::DatabaseConnection;
use crate::entity::{agent_approval_attempt as attempt, agent_approval_review as review};
use desk_diagnose_core::{
    approval_review::ReviewFormatIssue, subagent::reservation::ReviewCallAuthority,
};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DbErr, EntityTrait, QueryFilter, Set, sea_query::OnConflict,
};
fn invalid() -> DbErr {
    DbErr::Custom("inconsistent approval attempt".into())
}

pub(crate) fn authority(row: &review::Model) -> Result<ReviewCallAuthority, DbErr> {
    let a: ReviewCallAuthority =
        serde_json::from_str(row.call_authority_json.as_deref().ok_or_else(invalid)?)
            .map_err(|_| invalid())?;
    a.validate().map_err(|_| invalid())?;
    Ok(a)
}
pub(crate) async fn sync_on<C: ConnectionTrait>(db: &C, row: &review::Model) -> Result<(), DbErr> {
    let a = authority(row)?;
    let mut snapshot = row.clone();
    snapshot.decision_json = None;
    attempt::Entity::insert(attempt::ActiveModel {
        call_id: Set(a.attempt.call_id(&a.candidate_id)),
        candidate_id: Set(a.candidate_id.clone()),
        attempt_index: Set(i64::from(a.attempt.index)),
        delegation_id: Set(a.delegation_id.clone()),
        delegation_reservation_id: Set(row.delegation_reservation_id.clone()),
        provider_receipt_kind: Set(row.provider_receipt_kind.clone()),
        provider_receipt_id: Set(row.provider_receipt_id.clone()),
        snapshot_json: Set(serde_json::to_string(&snapshot).map_err(|_| invalid())?),
        response_complete: Set(false),
        format_issue_json: Set(None),
        usage_settlement_state: Set(row.usage_settlement_state.clone()),
        usage_reconcile_at_ms: Set(row.usage_reconcile_at_ms),
        created_at: Set(row.updated_at),
        updated_at: Set(row.updated_at),
        ..Default::default()
    })
    .on_conflict(
        OnConflict::column(attempt::Column::CallId)
            .update_columns([
                attempt::Column::SnapshotJson,
                attempt::Column::DelegationReservationId,
                attempt::Column::ProviderReceiptKind,
                attempt::Column::ProviderReceiptId,
                attempt::Column::UsageSettlementState,
                attempt::Column::UsageReconcileAtMs,
                attempt::Column::UpdatedAt,
            ])
            .to_owned(),
    )
    .exec_without_returning(db)
    .await?;
    Ok(())
}
pub(crate) async fn sync_current_on<C: ConnectionTrait>(
    db: &C,
    candidate: &str,
) -> Result<(), DbErr> {
    let row = review::Entity::find()
        .filter(review::Column::CandidateId.eq(candidate))
        .one(db)
        .await?
        .ok_or_else(invalid)?;
    sync_on(db, &row).await
}
pub(crate) async fn record_response(
    db: &DatabaseConnection,
    a: &ReviewCallAuthority,
    complete: bool,
    issue: Option<ReviewFormatIssue>,
) -> Result<(), DbErr> {
    let txn = crate::db::begin_write(db, review::Entity).await?;
    let row = review::Entity::find()
        .filter(review::Column::CandidateId.eq(&a.candidate_id))
        .one(&txn)
        .await?
        .ok_or_else(invalid)?;
    if row.status != "reviewing" || authority(&row)? != *a || row.provider_receipt_id.is_none() {
        return Err(invalid());
    }
    sync_on(&txn, &row).await?;
    let result = attempt::Entity::update_many()
        .set(attempt::ActiveModel {
            response_complete: Set(complete),
            format_issue_json: Set(issue
                .map(|v| serde_json::to_string(&v))
                .transpose()
                .map_err(|_| invalid())?),
            ..Default::default()
        })
        .filter(attempt::Column::CallId.eq(a.attempt.call_id(&a.candidate_id)))
        .filter(attempt::Column::ResponseComplete.eq(false))
        .exec(&txn)
        .await?;
    if result.rows_affected != 1 {
        return Err(invalid());
    }
    txn.commit().await?;
    Ok(())
}
