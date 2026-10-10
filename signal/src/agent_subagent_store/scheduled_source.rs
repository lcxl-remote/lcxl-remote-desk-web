//! A frozen scheduled source is revalidated under its publication write fence.

use super::*;
use crate::config::connection::DatabaseTransaction;

/// Owner/root/child controls are already held. Acquire a scheduled source fence
/// before locking the child's session or creating any new business authority.
pub(crate) async fn lock_child_source_on(
    txn: &DatabaseTransaction,
    session: &PersistedAgentSession,
) -> Result<(), DbErr> {
    let Some(binding) = session.agent_role.binding() else {
        return Ok(());
    };
    let row = group_row::Entity::find()
        .filter(group_row::Column::GroupId.eq(&binding.group_id))
        .filter(group_row::Column::RootConversationId.eq(&binding.root_conversation_id))
        .filter(group_row::Column::ActorId.eq(&session.actor_id))
        .filter(group_row::Column::DeviceId.eq(&session.device_id))
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    current_scheduled_source_on(txn, &row).await?;
    Ok(())
}

/// Owner/root/child controls precede this task fence. This records no grant and
/// cannot replace the child's own session holder or exact action authorization.
pub(crate) async fn current_scheduled_source_on(
    txn: &DatabaseTransaction,
    row: &group_row::Model,
) -> Result<Option<crate::schedule_store::CurrentDelegationSourceAuthority>, DbErr> {
    let group = decode_group(row)?;
    if !matches!(
        &group.source,
        desk_diagnose_core::subagent::DelegationSource::ScheduledOccurrence { .. }
    ) {
        return Ok(None);
    }
    let creation = decode_creation(row)?;
    let frozen = creation.scheduled_source.as_ref().ok_or_else(invalid)?;
    if group.source_admission != SourceAdmission::Open
        || creation
            .new_group(row.created_at, None)
            .map_err(|_| invalid())?
            .limits
            != group.limits
    {
        return Err(invalid());
    }
    let authority = crate::schedule_store::ScheduleStore::lock_delegation_source_authority(
        txn,
        group.actor_id.parse().map_err(|_| invalid())?,
        &group.device_id,
        frozen,
    )
    .await
    .map_err(|_| invalid())?;
    if authority.provenance() != &frozen.provenance
        || authority.verified_at() >= group.limits.deadline_ms
        || authority.deadline_ms() != frozen.deadline_ms().map_err(|_| invalid())?
    {
        return Err(invalid());
    }
    Ok(Some(authority))
}
