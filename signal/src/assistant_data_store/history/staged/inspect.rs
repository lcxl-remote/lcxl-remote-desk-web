use super::*;
use crate::assistant_data_store::unsigned;
use desk_diagnose_core::ai_assistant::source::HistoryRetentionProof;

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
            || row.original_sha256 != prepared.proof.original_content_sha256)
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
    let directory = capture
        .directory
        .ok_or(AssistantStoreError::Invalid("missing OSS source directory"))?;
    let existing_binding = binding::Entity::find()
        .filter(binding::Column::BindingId.eq(capture.device_ref))
        .one(txn)
        .await?;
    if existing_binding.as_ref().is_some_and(|row| {
        row.instance_id != directory.instance_id || row.audience != directory.audience
    }) {
        return Err(AdmissionFailure::Policy);
    }
    let mut new_parts = Vec::new();
    for (index, (fragment, source_id)) in prepared
        .index
        .fragments
        .iter()
        .zip(&prepared.source_ids)
        .enumerate()
    {
        let body =
            &prepared.retained_content[fragment.byte_start as usize..fragment.byte_end as usize];
        let hash = format!("{:x}", Sha256::digest(body.as_bytes()));
        if let Some(row) = history::Entity::find()
            .filter(history::Column::SourceId.eq(source_id))
            .one(txn)
            .await?
        {
            if row.actor_id != capture.actor_id
                || row.origin_id != prepared.origin_id
                || row.revision != signed(capture.revision)?
                || row.content_sha256 != hash
                || row.deleted_at_ms.is_some()
                || row.recorded_at_ms != signed(capture.recorded_at_unix_ms)?
                || row.statement_timezone != capture.statement_timezone
                || row.native_id != capture.native_id
                || row.role != capture.role
            {
                return Err(AdmissionFailure::Policy);
            }
            let proof: HistoryRetentionProof = serde_json::from_str(&row.retention_proof_json)
                .map_err(|_| AdmissionFailure::Policy)?;
            if proof.native_metadata_sha256 != prepared.proof.native_metadata_sha256
                || super::super::super::history_native::read(&row, &proof).is_err()
            {
                return Err(AdmissionFailure::Policy);
            }
        } else {
            new_parts.push((index, hash));
        }
    }
    if new_parts.is_empty() {
        return Ok(Inspection {
            existing_origin,
            existing_conversation,
            new_parts,
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
        runtime: Some(runtime_row),
        existing_binding,
    })
}
