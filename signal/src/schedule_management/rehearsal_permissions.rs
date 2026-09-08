//! Project verified historical scope without exposing reusable authorization handles.
use super::*;
use sea_orm::TransactionTrait;
use std::collections::BTreeSet;

pub(crate) async fn read(
    db: &DatabaseConnection,
    owner: i32,
    rehearsal_id: &str,
) -> Result<Response, ScheduleStoreError> {
    let txn = db.begin().await?;
    let reads = ScheduleStore::read_rehearsal_reads_on(&txn, owner, rehearsal_id).await?;
    let mut observations = Vec::new();
    let mut confirmed = BTreeSet::new();
    let mut unconfirmed: BTreeSet<_> = reads.unconfirmed_read_call_ids.into_iter().collect();
    let mut unclassified: BTreeSet<_> = reads.other_tool_call_ids.into_iter().collect();
    for item in reads.reads {
        if !confirmed.insert(item.tool_call_id.clone()) {
            return Err(ScheduleStoreError::Conflict);
        }
        observations.push(
            item.authority
                .review_observation(item.tool_call_id, &item.issued_by, item.completed_at)
                .ok_or(ScheduleStoreError::Invalid)?,
        );
    }
    let actions = ScheduleStore::read_rehearsal_actions_on(&txn, owner, rehearsal_id).await?;
    if actions.session_sha256 != reads.session_sha256 {
        return Err(ScheduleStoreError::Conflict);
    }
    unconfirmed.extend(actions.unconfirmed_tool_call_ids);
    unclassified.extend(actions.other_tool_call_ids);
    for item in actions.actions {
        let call_id = item.origin.tool_call_id;
        if !confirmed.insert(call_id.clone()) {
            return Err(ScheduleStoreError::Conflict);
        }
        observations.push(
            item.authority
                .review_observation(
                    call_id,
                    &item.issued_by,
                    i64::try_from(item.completed_at).map_err(|_| ScheduleStoreError::Invalid)?,
                )
                .ok_or(ScheduleStoreError::Invalid)?,
        );
    }
    if !confirmed.is_disjoint(&unconfirmed) {
        return Err(ScheduleStoreError::Conflict);
    }
    unclassified.retain(|id| !confirmed.contains(id) && !unconfirmed.contains(id));
    observations.sort_by(|a, b| a.tool_call_id.cmp(&b.tool_call_id));
    txn.commit().await?;
    Ok(Response::RehearsalPermissions {
        rehearsal_id: rehearsal_id.into(),
        observations,
        unconfirmed_tool_call_ids: unconfirmed.into_iter().collect(),
        unclassified_tool_call_ids: unclassified.into_iter().collect(),
    })
}
