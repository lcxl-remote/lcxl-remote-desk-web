//! Owner input identity and its actual retained fragments. A model label
//! cannot manufacture a source ID or replace an omitted original input.

use super::*;
use crate::entity::agent_assistant_origin as origin;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QuerySelect};

/// The occurrence's original requirement is independently authenticated. A
/// historical model summary cannot promote it to owner-authored chat input.
pub(super) fn published_on<'a>(
    db: &'a DatabaseTransaction,
    output: &'a ArchivedAssistantOutput,
    source: &'a DataEnvelope,
    now: u64,
) -> std::pin::Pin<
    Box<
        dyn std::future::Future<Output = Result<Option<SourceUsePolicy>, AssistantStoreError>>
            + Send
            + 'a,
    >,
> {
    desk_diagnose_core::future::send_phase(|| async move {
        let actor = &output.source_set.actor_id;
        let original = desk_diagnose_core::future::send_phase(|| {
            crate::schedule_store::ScheduleStore::published_input_retention_on(
                db,
                actor,
                &output.source_set.source_device_ref,
                &output.source_set.consumer_conversation_id,
                source,
            )
        })
        .await
        .map_err(|error| match error {
            crate::schedule_store::ScheduleStoreError::Backend(error) => {
                AssistantStoreError::Backend(error)
            }
            _ => AssistantStoreError::Inaccessible,
        })?;
        let Some(original) = original else {
            return Ok(None);
        };
        if original.policy.no_copy
            || !original.policy.derived_visible_text_allowed
            || original
                .policy
                .content_hard_expires_at_unix_ms
                .is_some_and(|until| until <= now)
        {
            return Ok(None);
        }
        Ok(Some(original.policy))
    })
}

/// Owner consent is resolved at completion; later copies use the frozen policy.
pub(super) fn owner_text_on<'a>(
    db: &'a DatabaseTransaction,
    output: &'a ArchivedAssistantOutput,
    source: &'a DataEnvelope,
) -> std::pin::Pin<
    Box<
        dyn std::future::Future<Output = Result<Option<SourceUsePolicy>, AssistantStoreError>>
            + Send
            + 'a,
    >,
> {
    desk_diagnose_core::future::send_phase(|| async move {
        let Some(native_id) = source.provenance.source_object_id.as_deref() else {
            return Ok(None);
        };
        let rows = origin::Entity::find()
            .filter(origin::Column::ActorId.eq(&output.source_set.actor_id))
            .filter(origin::Column::ConversationId.eq(&output.source_set.consumer_conversation_id))
            .filter(origin::Column::NativeId.eq(native_id))
            .filter(origin::Column::Kind.eq("input"))
            .limit(2)
            .all(db)
            .await?;
        let [original] = rows.as_slice() else {
            return Ok(None);
        };
        if original.device_ref != output.source_set.source_device_ref
            || original.original_sha256 != source.digest_sha256
            || !original.history_admitted
            || original.accepted_input_seq < 0
            || original.accepted_input_seq as u64 > output.actor_input_seq
            || !super::super::super::source_lifecycle::origin_readable_on(
                db,
                &output.source_set.actor_id,
                original,
            )
            .await?
        {
            return Ok(None);
        }
        let policy: SourceUsePolicy = serde_json::from_str(&original.policy_json)
            .map_err(|_| AssistantStoreError::Inaccessible)?;
        desk_diagnose_core::ai_assistant::source::validate_policy(&policy)
            .map_err(AssistantStoreError::Invalid)?;
        Ok((policy.history_retention_allowed
            && policy.derived_visible_text_allowed
            && !policy.no_copy)
            .then_some(policy))
    })
}
