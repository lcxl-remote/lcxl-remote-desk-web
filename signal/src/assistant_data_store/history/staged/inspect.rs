use super::*;
use crate::assistant_data_store::unsigned;

pub(super) async fn inspect_on(
    txn: &DatabaseTransaction,
    capture: &HistoryCapture<'_>,
    prepared: &PreparedHistoryCapture,
) -> Result<Inspection, AdmissionFailure> {
    if !super::super::super::source_lifecycle::conversation_readable_on(
        txn,
        capture.actor_id,
        capture.conversation_id,
        capture.root_conversation_id,
    )
    .await?
    {
        return Err(AdmissionFailure::Policy);
    }
    let kind = enum_string(&capture.kind)?;
    let existing_origin = origin::Entity::find()
        .filter(origin::Column::OriginId.eq(&prepared.origin_id))
        .one(txn)
        .await?;
    if let Some(row) = &existing_origin
        && (row.deleted_at_ms.is_some()
            || row.actor_id != capture.actor_id
            || row.device_ref != capture.device_ref
            || row.conversation_id != capture.conversation_id
            || row.native_id != capture.native_id
            || row.accepted_input_seq != signed(capture.accepted_input_seq)?
            || row.kind != kind
            || row.original_sha256
                != format!(
                    "{:x}",
                    <sha2::Sha256 as sha2::Digest>::digest(capture.content.as_bytes())
                ))
    {
        return Err(AdmissionFailure::Policy);
    }
    let existing_conversation = conversation::Entity::find()
        .filter(conversation::Column::ConversationId.eq(capture.conversation_id))
        .one(txn)
        .await?;
    if let Some(row) = &existing_conversation
        && (row.actor_id != capture.actor_id
            || row.device_ref != capture.device_ref
            || row.root_conversation_id != capture.root_conversation_id
            || matches!(row.status.as_str(), "user_deleted" | "expired"))
    {
        return Err(AdmissionFailure::Policy);
    }
    let existing_binding = if capture.device_ref.is_empty() {
        None
    } else {
        let directory = capture
            .directory
            .ok_or(AssistantStoreError::Invalid("missing OSS source directory"))?;
        let existing = binding::Entity::find()
            .filter(binding::Column::BindingId.eq(capture.device_ref))
            .one(txn)
            .await?;
        if existing.as_ref().is_some_and(|row| {
            row.instance_id != directory.instance_id || row.audience != directory.audience
        }) {
            return Err(AdmissionFailure::Policy);
        }
        existing
    };
    let mut new_parts = Vec::new();
    let mut existing_ids = Vec::with_capacity(prepared.index.fragments.len());
    for (index, fragment) in prepared.index.fragments.iter().enumerate() {
        let body =
            &prepared.retained_content[fragment.byte_start as usize..fragment.byte_end as usize];
        if let Some(row) = history::Entity::find()
            .filter(history::Column::ActorId.eq(capture.actor_id))
            .filter(history::Column::OriginId.eq(&prepared.origin_id))
            .filter(history::Column::FragmentIndex.eq(
                i32::try_from(fragment.fragment_index).map_err(|_| AdmissionFailure::Capacity)?,
            ))
            .one(txn)
            .await?
        {
            if row.actor_id != capture.actor_id
                || row.origin_id != prepared.origin_id
                || row.revision != signed(capture.revision)?
                || row.content != body
                || row.deleted_at_ms.is_some()
                || row.recorded_at_ms != signed(capture.recorded_at_unix_ms)?
                || row.statement_timezone != capture.statement_timezone
                || row.native_id != capture.native_id
                || row.role != capture.role
            {
                return Err(AdmissionFailure::Policy);
            }
            // A replay must carry the same native metadata as the stored row.
            let state_json = capture
                .native
                .map(serde_json::to_string)
                .transpose()
                .map_err(|_| AdmissionFailure::Policy)?;
            if row.state_json != state_json
                || super::super::super::history_native::read(&row).is_err()
            {
                return Err(AdmissionFailure::Policy);
            }
            existing_ids.push(Some(row.id));
        } else {
            existing_ids.push(None);
            new_parts.push(index);
        }
    }
    if new_parts.is_empty() {
        return Ok(Inspection {
            existing_origin,
            existing_conversation,
            new_parts,
            existing_ids: existing_ids.clone(),
            runtime: None,
            existing_binding,
        });
    }
    if existing_origin
        .as_ref()
        .is_some_and(|row| row.history_admitted)
    {
        // An archived native unit cannot be re-created from a later save or replay.
        return Err(AdmissionFailure::Policy);
    }
    let runtime_row = runtime::Entity::find()
        .filter(runtime::Column::ActorId.eq(capture.actor_id))
        .one(txn)
        .await?
        .ok_or(AssistantStoreError::Invalid(
            "missing assistant runtime row",
        ))?;
    if capture.accepted_input_seq > unsigned(runtime_row.input_seq)? {
        return Err(AdmissionFailure::Policy);
    }
    Ok(Inspection {
        existing_origin,
        existing_conversation,
        new_parts,
        existing_ids: existing_ids.clone(),
        runtime: Some(runtime_row),
        existing_binding,
    })
}
