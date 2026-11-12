//! Project a pending fresh-task proposal for the AI reviewer and commit its verdict.
use super::{ProposalDecider, ScheduleStore, ScheduleStoreError, entity};
use crate::config::connection::DatabaseTransaction;
use desk_diagnose_core::{schedule::review::ScheduleProposal, session::PersistedAgentSession};
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter};

/// The proposal waiting in `session`'s turn, or `None` once it was decided,
/// deleted or belongs to another conversation.
pub(crate) async fn review_proposal_on<C: ConnectionTrait + crate::config::ConfigConnection>(
    db: &C,
    session: &PersistedAgentSession,
    schedule_id: &str,
) -> Result<Option<ScheduleProposal>, ScheduleStoreError> {
    let Ok(owner) = session.actor_id.parse::<i32>() else {
        return Ok(None);
    };
    let Some(task) = entity::Entity::find()
        .filter(entity::Column::OwnerUserId.eq(owner))
        .filter(entity::Column::ScheduleId.eq(schedule_id))
        .filter(entity::Column::Status.eq("pending_review"))
        .filter(entity::Column::Kind.eq("fresh_task"))
        .filter(entity::Column::ProposalConversationId.eq(&session.conversation_id))
        .one(db)
        .await?
    else {
        return Ok(None);
    };
    Ok(Some(ScheduleProposal {
        schedule_id: task.schedule_id,
        revision: task.revision,
        owner_id: session.actor_id.clone(),
        target_device_id: task.target_device_id,
        title: task.title,
        prompt: task.prompt,
        spec: desk_diagnose_core::schedule::parse_json(&task.spec_json)
            .map_err(|_| ScheduleStoreError::Invalid)?,
        proposal_conversation_id: session.conversation_id.clone(),
        input_revision: session.input_revision,
        created_at_unix_ms: u64::try_from(task.created_at)
            .map_err(|_| ScheduleStoreError::Invalid)?,
    }))
}

/// Apply the reviewer's verdict to the exact reviewed revision in the
/// caller's review-settlement transaction.
pub(crate) async fn decide_by_ai_on(
    txn: &DatabaseTransaction,
    owner: &str,
    proposal: &ScheduleProposal,
    approve: bool,
    reason: &str,
) -> Result<(), ScheduleStoreError> {
    let owner = owner
        .parse::<i32>()
        .map_err(|_| ScheduleStoreError::Invalid)?;
    ScheduleStore::decide_proposal_on(
        txn,
        owner,
        &proposal.schedule_id,
        proposal.revision,
        approve,
        &ProposalDecider::Ai {
            reason: reason.to_owned(),
        },
    )
    .await?;
    Ok(())
}
