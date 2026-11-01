use super::*;
use crate::entity::agent_assistant_model_parent as link;
use desk_diagnose_core::ai_assistant::source_registry::NativeDerivedInputPolicy;
use sea_orm::QueryOrder;

/// Indexed lookup of the exact immutable parent. Every reference and policy
/// summary is checked against the original row, never against a guessed ID.
#[cfg(test)]
pub(crate) async fn find_parent_on<C: ConnectionTrait>(
    db: &C,
    actor: &str,
    conversation: &str,
    reference: &NativeDerivedInputPolicy,
) -> Result<StoredOutput, AssistantStoreError> {
    find_parent_reusing_on(db, actor, conversation, reference, None).await
}

async fn find_parent_reusing_on<C: ConnectionTrait>(
    db: &C,
    actor: &str,
    conversation: &str,
    reference: &NativeDerivedInputPolicy,
    known: Option<&StoredOutput>,
) -> Result<StoredOutput, AssistantStoreError> {
    reference.validate().map_err(AssistantStoreError::Invalid)?;
    let row = archived::Entity::find()
        .filter(archived::Column::ActorId.eq(actor))
        .filter(archived::Column::ConversationId.eq(conversation))
        .filter(archived::Column::PhysicalCallId.eq(&reference.physical_call_id))
        .one(db)
        .await?
        .ok_or(AssistantStoreError::Inaccessible)?;
    let stored = match known {
        Some(known) if known.row == row => return validated_reference(reference, known.clone()),
        _ => decode_row(row)?,
    };
    let stored = validated_reference(reference, stored)?;
    verify_on(db, &stored.proof).await?;
    Ok(stored)
}

fn validated_reference(
    reference: &NativeDerivedInputPolicy,
    stored: StoredOutput,
) -> Result<StoredOutput, AssistantStoreError> {
    reference
        .validate_archive(&stored.proof)
        .map_err(AssistantStoreError::Invalid)?;
    Ok(stored)
}

pub(super) async fn prepare_on<C: ConnectionTrait>(
    db: &C,
    proof: &ArchivedAssistantOutput,
    known: &[StoredOutput],
) -> Result<Vec<link::Model>, AssistantStoreError> {
    proof
        .input_policy_graph
        .validate_native_closure(&proof.sources.input_snapshot)
        .map_err(AssistantStoreError::Invalid)?;
    let consumer_fingerprint = proof.fingerprint().map_err(AssistantStoreError::Invalid)?;
    let mut rows = Vec::with_capacity(proof.input_policy_graph.parents.len());
    for reference in &proof.input_policy_graph.parents {
        let known = known
            .iter()
            .find(|value| value.row.physical_call_id == reference.physical_call_id);
        let parent = find_parent_reusing_on(
            db,
            &proof.source_set.actor_id,
            &proof.source_set.consumer_conversation_id,
            reference,
            known,
        )
        .await?;
        if parent.proof.source_set.source_device_ref != proof.source_set.source_device_ref {
            return Err(AssistantStoreError::Inaccessible);
        }
        // This transaction holds the SQLite writer, so a parent verified earlier in it
        // and re-read unchanged cannot have new links or a new native receipt.
        if known.is_none_or(|known| known.row != parent.row) {
            crate::model_egress_store::SignalModelEgressStore::verify_retained_output_on(
                db,
                &parent.proof.source_set,
                &parent.proof.sources,
                &parent.proof.output,
            )
            .await?;
        }
        rows.push(expected_row(proof, reference, &consumer_fingerprint)?);
    }
    // A new archive cannot silently absorb an orphan or partial prior pin set.
    let existing = rows_on(
        db,
        &proof.source_set.actor_id,
        &proof.source_set.physical_call_id,
        rows.len(),
    )
    .await?;
    if !existing.is_empty() {
        return Err(AssistantStoreError::Inaccessible);
    }
    Ok(rows)
}

pub(crate) async fn verify_on<C: ConnectionTrait>(
    db: &C,
    proof: &ArchivedAssistantOutput,
) -> Result<(), AssistantStoreError> {
    let fingerprint = proof.fingerprint().map_err(AssistantStoreError::Invalid)?;
    verify_fingerprint_on(db, proof, &fingerprint).await
}

/// The caller has checked this fingerprint against the complete unchanged row.
pub(super) async fn verify_fingerprint_on<C: ConnectionTrait>(
    db: &C,
    proof: &ArchivedAssistantOutput,
    consumer_fingerprint: &str,
) -> Result<(), AssistantStoreError> {
    let rows = rows_on(
        db,
        &proof.source_set.actor_id,
        &proof.source_set.physical_call_id,
        proof.input_policy_graph.parents.len(),
    )
    .await?;
    if rows.len() != proof.input_policy_graph.parents.len() {
        return Err(AssistantStoreError::Inaccessible);
    }
    let mut seen = std::collections::BTreeSet::new();
    for mut row in rows {
        if row.id <= 0 || !seen.insert(row.parent_call_id.clone()) {
            return Err(AssistantStoreError::Inaccessible);
        }
        row.id = 0;
        let reference = proof
            .input_policy_graph
            .parents
            .iter()
            .find(|parent| parent.physical_call_id == row.parent_call_id)
            .ok_or(AssistantStoreError::Inaccessible)?;
        if row != expected_row(proof, reference, consumer_fingerprint)? {
            return Err(AssistantStoreError::Inaccessible);
        }
    }
    Ok(())
}

async fn rows_on<C: ConnectionTrait>(
    db: &C,
    actor: &str,
    child: &str,
    expected: usize,
) -> Result<Vec<link::Model>, AssistantStoreError> {
    Ok(link::Entity::find()
        .filter(link::Column::ActorId.eq(actor))
        .filter(link::Column::ConsumerCallId.eq(child))
        .order_by_asc(link::Column::ParentCallId)
        .limit(expected as u64 + 1)
        .all(db)
        .await?)
}

fn expected_row(
    proof: &ArchivedAssistantOutput,
    reference: &NativeDerivedInputPolicy,
    consumer_fingerprint: &str,
) -> Result<link::Model, AssistantStoreError> {
    Ok(link::Model {
        id: 0,
        actor_id: proof.source_set.actor_id.clone(),
        conversation_id: proof.source_set.consumer_conversation_id.clone(),
        consumer_call_id: proof.source_set.physical_call_id.clone(),
        parent_call_id: reference.physical_call_id.clone(),
        consumer_proof_sha256: consumer_fingerprint.into(),
        parent_proof_sha256: reference.source_archive_sha256.clone(),
        parent_output_envelope_id: reference.output_envelope_id.clone(),
        parent_output_label_sha256: reference.output_label_sha256.clone(),
        created_at_ms: signed(proof.sources.evidence.completed_at_unix_ms)?,
    })
}

pub(crate) fn metadata_bytes(row: &link::Model) -> Result<i64, AssistantStoreError> {
    let fields = [
        &row.actor_id,
        &row.conversation_id,
        &row.consumer_call_id,
        &row.parent_call_id,
        &row.consumer_proof_sha256,
        &row.parent_proof_sha256,
        &row.parent_output_envelope_id,
        &row.parent_output_label_sha256,
    ];
    let size = fields
        .into_iter()
        .try_fold(2 * 8_usize, |size, field| size.checked_add(field.len()))
        .ok_or(AssistantStoreError::Capacity)?;
    i64::try_from(size).map_err(|_| AssistantStoreError::Capacity)
}
