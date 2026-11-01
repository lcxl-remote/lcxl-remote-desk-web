//! Accepted-output provenance survives the original operation's short lifetime.
//! Callers hold their native actor and output fences; reads grant no current use.

use super::{AssistantStoreError, signed, unsigned};
use crate::config::connection::DatabaseTransaction;
use crate::entity::agent_assistant_model_output as archived;
use desk_agent_protocol::{
    ai_assistant::user_instructions::AssistantDataPolicy, data_lineage::DataEnvelope,
};
use desk_diagnose_core::ai_assistant::output_archive::ArchivedAssistantOutput;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, QuerySelect,
};

pub(crate) mod gc;
pub(crate) mod parents;

#[derive(Clone)]
pub(crate) struct StoredOutput {
    pub row: archived::Model,
    pub proof: Box<ArchivedAssistantOutput>,
}

/// Resolve historical policy only from original accepted outputs in this
/// owner-bound native context. Missing producer kinds remain unresolved.
pub(crate) async fn input_policy_graph_on<C: ConnectionTrait>(
    db: &C,
    set: &desk_diagnose_core::ai_assistant::native_export::AssistantNativeSourceSet,
    snapshot: &desk_diagnose_core::model_egress::NativeModelInputSnapshot,
    registration: &desk_diagnose_core::ai_assistant::source_registry::NativeInputPolicyRegistration,
) -> Result<
    (
        desk_diagnose_core::ai_assistant::source_registry::NativeInputPolicyGraph,
        Vec<StoredOutput>,
    ),
    AssistantStoreError,
> {
    use desk_diagnose_core::ai_assistant::source_registry::{
        NativeInputPolicyGraph, is_native_output_policy_source,
    };
    let mut graph =
        NativeInputPolicyGraph::empty(snapshot, &set.actor_id, &set.consumer_conversation_id)
            .map_err(AssistantStoreError::Invalid)?;
    let mut seen = std::collections::BTreeSet::new();
    let mut parents = Vec::new();
    for source in snapshot.policy_sources() {
        if !is_native_output_policy_source(source) || !seen.insert(&source.envelope_id) {
            continue;
        }
        let Some(parent) = find_on(db, &set.consumer_conversation_id, source).await? else {
            continue;
        };
        if parent.proof.source_set.actor_id != set.actor_id
            || parent.proof.source_set.source_device_ref != set.source_device_ref
            || parent.proof.source_set.physical_call_id == set.physical_call_id
        {
            return Err(AssistantStoreError::Inaccessible);
        }
        let native =
            crate::model_egress_store::SignalModelEgressStore::read_context_output_reusing_on(
                db,
                &set.consumer_conversation_id,
                source,
                Some(&parent),
            )
            .await?;
        if native != parent.proof.sources {
            return Err(AssistantStoreError::Inaccessible);
        }
        graph
            .include_archive(&parent.proof)
            .map_err(AssistantStoreError::Invalid)?;
        parents.push(parent);
    }
    graph
        .validate(snapshot, registration)
        .map_err(AssistantStoreError::Invalid)?;
    Ok((graph, parents))
}

/// All original native/source reads happen before the caller updates its native
/// acceptance. Finalization only writes quota and bounded provenance metadata.
pub(crate) struct PendingOutputArchive<'a> {
    db: &'a DatabaseTransaction,
    row: Option<archived::Model>,
    parents: Vec<crate::entity::agent_assistant_model_parent::Model>,
    charge: i64,
    maximum_metadata_bytes: u64,
}

impl PendingOutputArchive<'_> {
    pub(crate) async fn finalize_on(self: Box<Self>) -> Result<(), AssistantStoreError> {
        let Self {
            db,
            row,
            parents,
            charge,
            maximum_metadata_bytes,
        } = *self;
        let Some(row) = row else {
            return Ok(());
        };
        super::projection::adjust_metadata_on(
            db,
            &row.actor_id,
            charge,
            Some(maximum_metadata_bytes),
        )
        .await?;
        let mut active: archived::ActiveModel = row.into();
        active.id = sea_orm::ActiveValue::NotSet;
        decode_row(active.insert(db).await?)?;
        for row in parents {
            let mut active: crate::entity::agent_assistant_model_parent::ActiveModel = row.into();
            active.id = sea_orm::ActiveValue::NotSet;
            active.insert(db).await?;
        }
        Ok(())
    }
}

/// Mandatory source provenance is prepared in the original receipt transaction.
/// Any later quota/insert failure must roll back its native acceptance as well.
pub(crate) async fn prepare_on<'a>(
    db: &'a DatabaseTransaction,
    proof: &ArchivedAssistantOutput,
    policy: &AssistantDataPolicy,
    known: &[StoredOutput],
) -> Result<Box<PendingOutputArchive<'a>>, AssistantStoreError> {
    proof.validate().map_err(AssistantStoreError::Invalid)?;
    policy.validate().map_err(AssistantStoreError::Invalid)?;
    if proof.source_set.context.policy_revision != policy.revision {
        return Err(AssistantStoreError::Inaccessible);
    }
    let grants = super::native_export::load_call_set_on(db, &proof.source_set).await?;
    let grants = grants
        .into_iter()
        .map(|stored| *stored.proof)
        .collect::<Vec<_>>();
    if grants
        .iter()
        .any(|grant| grant.policy_revision != policy.revision)
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    proof
        .validate_grants(&grants)
        .map_err(AssistantStoreError::Invalid)?;
    verify_native_on(db, proof).await?;
    if let Some(row) = archived::Entity::find()
        .filter(archived::Column::PhysicalCallId.eq(&proof.source_set.physical_call_id))
        .one(db)
        .await?
    {
        let stored = decode_row(row)?;
        if stored.proof.as_ref() != proof {
            return Err(AssistantStoreError::Inaccessible);
        }
        parents::verify_fingerprint_on(db, &stored.proof, &stored.row.proof_sha256).await?;
        return Ok(Box::new(PendingOutputArchive {
            db,
            row: None,
            parents: Vec::new(),
            charge: 0,
            maximum_metadata_bytes: policy.max_metadata_bytes,
        }));
    }
    let evidence = &proof.sources.evidence;
    let row = archived::Model {
        id: 0,
        actor_id: proof.source_set.actor_id.clone(),
        conversation_id: evidence.context_conversation_id.clone(),
        device_ref: proof.source_set.source_device_ref.clone(),
        physical_call_id: evidence.physical_call_id.clone(),
        output_envelope_id: evidence.output_envelope_id.clone(),
        output_digest_sha256: evidence.output_digest_sha256.clone(),
        output_label_sha256: evidence.output_label_sha256.clone(),
        input_lineage_sha256: evidence.input_lineage_sha256.clone(),
        actor_input_seq: signed(proof.actor_input_seq)?,
        input_revision: signed(proof.input_revision)?,
        input_recorded_at_ms: signed(proof.input_recorded_at_unix_ms)?,
        statement_timezone: proof.statement_timezone.clone(),
        completed_at_ms: signed(evidence.completed_at_unix_ms)?,
        proof_sha256: proof.fingerprint().map_err(AssistantStoreError::Invalid)?,
        proof_json: serde_json::to_string(proof)
            .map_err(|_| AssistantStoreError::Invalid("invalid archived model metadata"))?,
    };
    let parents = parents::prepare_on(db, proof, known).await?;
    let mut charge = metadata_bytes(&row)?;
    for parent in &parents {
        charge = charge
            .checked_add(parents::metadata_bytes(parent)?)
            .ok_or(AssistantStoreError::Capacity)?;
    }
    Ok(Box::new(PendingOutputArchive {
        db,
        row: Some(row),
        parents,
        charge,
        maximum_metadata_bytes: policy.max_metadata_bytes,
    }))
}

/// The caller authenticates the original native context before this indexed
/// metadata lookup. Source access and task authority are checked separately.
pub(crate) async fn find_on<C: ConnectionTrait>(
    db: &C,
    conversation: &str,
    output: &DataEnvelope,
) -> Result<Option<StoredOutput>, AssistantStoreError> {
    find_reusing_on(db, conversation, output, None).await
}

/// Keep the original lookup and parent reads even when a caller already owns
/// this archive. Only an exactly identical raw row may reuse the decoded proof.
pub(crate) async fn find_reusing_on<C: ConnectionTrait>(
    db: &C,
    conversation: &str,
    output: &DataEnvelope,
    known: Option<&StoredOutput>,
) -> Result<Option<StoredOutput>, AssistantStoreError> {
    let rows = archived::Entity::find()
        .filter(archived::Column::ConversationId.eq(conversation))
        .filter(archived::Column::OutputEnvelopeId.eq(&output.envelope_id))
        .filter(archived::Column::OutputDigestSha256.eq(&output.digest_sha256))
        .limit(2)
        .all(db)
        .await?;
    if rows.len() > 1 {
        return Err(AssistantStoreError::Inaccessible);
    }
    let Some(row) = rows.into_iter().next() else {
        return Ok(None);
    };
    let stored = match known {
        Some(known) if known.row == row => known.clone(),
        _ => decode_row(row)?,
    };
    if &stored.proof.output != output {
        return Err(AssistantStoreError::Inaccessible);
    }
    parents::verify_on(db, &stored.proof).await?;
    Ok(Some(stored))
}

pub(crate) fn decode_row(row: archived::Model) -> Result<StoredOutput, AssistantStoreError> {
    let proof = Box::new(
        ArchivedAssistantOutput::decode(&row.proof_json).map_err(AssistantStoreError::Invalid)?,
    );
    let evidence = &proof.sources.evidence;
    if row.proof_sha256 != proof.fingerprint().map_err(AssistantStoreError::Invalid)?
        || row.actor_id != proof.source_set.actor_id
        || row.conversation_id != evidence.context_conversation_id
        || row.device_ref != proof.source_set.source_device_ref
        || row.physical_call_id != evidence.physical_call_id
        || row.output_envelope_id != evidence.output_envelope_id
        || row.output_digest_sha256 != evidence.output_digest_sha256
        || row.output_label_sha256 != evidence.output_label_sha256
        || row.input_lineage_sha256 != evidence.input_lineage_sha256
        || unsigned(row.actor_input_seq)? != proof.actor_input_seq
        || unsigned(row.input_revision)? != proof.input_revision
        || unsigned(row.input_recorded_at_ms)? != proof.input_recorded_at_unix_ms
        || row.statement_timezone != proof.statement_timezone
        || unsigned(row.completed_at_ms)? != evidence.completed_at_unix_ms
    {
        return Err(AssistantStoreError::Invalid(
            "archived model metadata binding changed",
        ));
    }
    Ok(StoredOutput { row, proof })
}

pub(crate) fn metadata_bytes(row: &archived::Model) -> Result<i64, AssistantStoreError> {
    let strings = [
        &row.actor_id,
        &row.conversation_id,
        &row.device_ref,
        &row.physical_call_id,
        &row.output_envelope_id,
        &row.output_digest_sha256,
        &row.output_label_sha256,
        &row.input_lineage_sha256,
        &row.statement_timezone,
        &row.proof_sha256,
        &row.proof_json,
    ];
    let bytes = strings
        .into_iter()
        .try_fold(5 * 8_usize, |size, value| size.checked_add(value.len()))
        .ok_or(AssistantStoreError::Capacity)?;
    i64::try_from(bytes).map_err(|_| AssistantStoreError::Capacity)
}

async fn verify_native_on<C: ConnectionTrait>(
    db: &C,
    proof: &ArchivedAssistantOutput,
) -> Result<(), AssistantStoreError> {
    use crate::entity::model_egress_receipt as native;
    let row = native::Entity::find_by_id(&proof.source_set.physical_call_id)
        .one(db)
        .await?
        .ok_or(AssistantStoreError::Inaccessible)?;
    if row.state != crate::model_egress_store::STATE_DISPATCH_INTENT
        || row.completed_at.is_some()
        || row.context_conversation_id.as_deref()
            != Some(proof.source_set.consumer_conversation_id.as_str())
        || row
            .assistant_source_set_json
            .as_deref()
            .map(desk_diagnose_core::ai_assistant::native_export::AssistantNativeSourceSet::decode)
            .transpose()
            .map_err(AssistantStoreError::Invalid)?
            .as_ref()
            != Some(&proof.source_set)
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    let (audit, inputs, input_snapshot) = crate::model_egress_store::receipt_inputs(&row)?;
    if proof.sources.evidence.completed_at_unix_ms > chrono::Utc::now().timestamp_millis() as u64 {
        return Err(AssistantStoreError::Inaccessible);
    }
    let evidence = desk_diagnose_core::model_egress::NativeModelOutputEvidence::from_receipt(
        &row.receipt_id,
        &proof.source_set.consumer_conversation_id,
        &row.export_authorization_id,
        unsigned(row.authorized_at.timestamp_millis())?,
        proof.sources.evidence.completed_at_unix_ms,
        &audit,
        &inputs,
        &input_snapshot,
        &proof.output,
    )
    .map_err(AssistantStoreError::Invalid)?;
    let sources = desk_diagnose_core::model_egress::NativeModelOutputSources {
        evidence,
        inputs,
        input_snapshot,
    };
    if sources != proof.sources {
        return Err(AssistantStoreError::Inaccessible);
    }
    Ok(())
}
