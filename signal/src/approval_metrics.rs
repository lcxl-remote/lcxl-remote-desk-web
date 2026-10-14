//! Approval statistics observe committed metadata, never prompt or replay content.
use crate::config::connection::DatabaseConnection;
use crate::entity::agent_approval_review as review;
use desk_diagnose_core::{
    approval_observation::{ReviewReason, ReviewTracking},
    model_observability::ObservationPhase,
};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
pub fn submit(tracking: &ReviewTracking, phase: ObservationPhase, sequence: u32, now: i64) {
    crate::model_metrics::runtime::submit(tracking.event(phase, sequence, now));
}
pub fn committed(db: &DatabaseConnection, candidate: &str) {
    let db = db.clone();
    let candidate = candidate.to_owned();
    tokio::spawn(async move {
        if let Ok(Some(row)) = review::Entity::find()
            .filter(review::Column::CandidateId.eq(candidate))
            .one(&db)
            .await
            && let Some(tracking) = row
                .statistics_json
                .as_deref()
                .and_then(|v| serde_json::from_str::<ReviewTracking>(v).ok())
        {
            let phase = if tracking.snapshot.terminal() {
                ObservationPhase::Completed
            } else {
                ObservationPhase::Started
            };
            submit(
                &tracking,
                phase,
                if tracking.snapshot.terminal() { 2 } else { 0 },
                chrono::Utc::now().timestamp_millis(),
            );
        }
    });
}
pub fn settled_json(
    row: &review::Model,
    tracking: Option<ReviewTracking>,
    status: &str,
    reason: Option<ReviewReason>,
    now: i64,
) -> Option<String> {
    let original: ReviewTracking = serde_json::from_str(row.statistics_json.as_deref()?).ok()?;
    let value = tracking
        .filter(|v| v.object_id == original.object_id)
        .unwrap_or(original)
        .settled(status, reason, now);
    serde_json::to_string(&value).ok()
}

pub async fn unavailable_on<C: crate::config::ConfigConnection>(
    db: &C,
    session: &desk_diagnose_core::session::PersistedAgentSession,
    request: &desk_diagnose_core::dynamic_run::PermissionRequest,
    decision: &desk_diagnose_core::dynamic_run::PermissionDecidedEvent,
    now: i64,
) -> Vec<ReviewTracking> {
    use crate::entity::agent_approval_delegation as delegation;
    let root = session.approval_delegation_conversation_id();
    let Some(row) = delegation::Entity::find()
        .filter(delegation::Column::ConversationId.eq(root))
        .filter(delegation::Column::ActorId.eq(&session.actor_id))
        .filter(delegation::Column::DeviceId.eq(&session.device_id))
        .one(db)
        .await
        .ok()
        .flatten()
    else {
        return vec![];
    };
    let Ok(delegation) = serde_json::from_str::<
        desk_diagnose_core::approval_delegation::ApprovalDelegation,
    >(&row.state_json) else {
        return vec![];
    };
    let mut values = Vec::new();
    for item in &request.items {
        let Some(code) = decision
            .decision_source
            .system_reason_for_item(&item.item_id)
        else {
            continue;
        };
        let reason = desk_diagnose_core::approval_review::system_review_reason_from_code(code);
        let Ok(source) = desk_diagnose_core::approval_review::permission_review_source_id(
            &request.request_id,
            &item.item_id,
        ) else {
            continue;
        };
        let existing = review::Entity::find()
            .filter(review::Column::ConversationId.eq(&session.conversation_id))
            .filter(review::Column::ActorId.eq(&session.actor_id))
            .filter(review::Column::DeviceId.eq(&session.device_id))
            .filter(review::Column::SourceKind.eq("permission_item"))
            .filter(review::Column::SourceId.eq(&source))
            .one(db)
            .await;
        if !matches!(existing, Ok(None)) {
            continue;
        }
        let value = ReviewTracking::queued(
            &session.actor_id,
            &session.device_id,
            &session.conversation_id,
            root,
            &delegation.delegation_id,
            delegation.revision,
            "permission_item",
            &source,
            now,
        )
        .settled("unavailable", Some(reason), now);
        values.push(value);
    }
    values
}

pub async fn unavailable_reason_on<C: crate::config::ConfigConnection>(db: &C) -> ReviewReason {
    match crate::approval_model_provider::load(db).await {
        Ok(config) if config.destination_identity().is_err() => ReviewReason::Configuration,
        _ => ReviewReason::Unknown,
    }
}
