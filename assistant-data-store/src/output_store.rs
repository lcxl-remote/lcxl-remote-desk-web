//! Accepted-output proofs share one copy of each growing metadata item per
//! conversation, so stored bytes stay proportional to what each call adds.
//! Fingerprints keep covering the complete rebuilt proof. Readers that only
//! need an unchanged parent's identity use its head and never load the items.

use crate::{
    AssistantStoreError,
    entity::{agent_assistant_model_input_item as item, agent_assistant_model_output as archived},
};
use desk_agent_protocol::{ai_assistant::source::SourceUsePolicy, data_lineage::DataEnvelope};
use desk_diagnose_core::{
    ai_assistant::{
        output_archive::ArchivedAssistantOutput, seam::AssistantReadRoot,
        source_registry::NativeDerivedInputPolicy,
    },
    model_egress::sink_metadata_sha256,
};
use sea_orm::{
    ActiveValue::Set, ColumnTrait, ConnectionTrait, EntityTrait, FromQueryResult, QueryFilter,
    QueryOrder, QuerySelect,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// Each kind is one list inside the serialized proof.
const LISTS: [(i16, &[&str]); 5] = [
    (1, &["sources", "inputs"]),
    (2, &["sources", "input_snapshot", "envelopes"]),
    (3, &["sources", "input_snapshot", "exports"]),
    (4, &["input_policies", "leaves"]),
    (5, &["input_policy_graph", "parents"]),
];

const INVALID: AssistantStoreError = AssistantStoreError::Invalid("invalid stored output proof");

fn list_mut<'a>(value: &'a mut Value, path: &[&str]) -> Result<&'a mut Value, AssistantStoreError> {
    path.iter()
        .try_fold(value, |value, key| value.get_mut(key))
        .ok_or(INVALID)
}

/// Contiguous ordinals become one `[first, last]` pair; order is preserved.
fn ranges(ordinals: &[i64]) -> Vec<(i64, i64)> {
    let mut ranges: Vec<(i64, i64)> = Vec::new();
    for ordinal in ordinals {
        match ranges.last_mut() {
            Some((_, last)) if last.checked_add(1) == Some(*ordinal) => *last = *ordinal,
            _ => ranges.push((*ordinal, *ordinal)),
        }
    }
    ranges
}

/// A proof ready to store. New items are inserted when the archive is finalized.
pub struct EncodedProof {
    pub proof_json: String,
    pub parent_summary_json: String,
    pub new_items: Vec<item::Model>,
}

impl EncodedProof {
    pub fn item_bytes(&self) -> Result<i64, AssistantStoreError> {
        self.new_items.iter().try_fold(0_i64, |total, row| {
            total
                .checked_add(item_metadata_bytes(row)?)
                .ok_or(AssistantStoreError::Capacity)
        })
    }
}

pub fn item_metadata_bytes(row: &item::Model) -> Result<i64, AssistantStoreError> {
    [
        &row.actor_id,
        &row.conversation_id,
        &row.item_sha256,
        &row.item_json,
    ]
    .into_iter()
    .try_fold(2 * 8_usize, |size, field| size.checked_add(field.len()))
    .and_then(|size| i64::try_from(size).ok())
    .ok_or(AssistantStoreError::Capacity)
}

/// The caller holds the conversation's writer fence, which also serializes ordinals.
pub async fn encode_on<C: ConnectionTrait>(
    db: &C,
    proof: &ArchivedAssistantOutput,
) -> Result<EncodedProof, AssistantStoreError> {
    let actor = &proof.source_set.actor_id;
    let conversation = &proof.source_set.consumer_conversation_id;
    let mut value = serde_json::to_value(proof).map_err(|_| INVALID)?;
    let mut new_items = Vec::new();
    for (kind, path) in LISTS {
        let list = list_mut(&mut value, path)?;
        let entries = list.as_array().ok_or(INVALID)?;
        let mut known: BTreeMap<String, i64> = item::Entity::find()
            .select_only()
            .column(item::Column::ItemSha256)
            .column(item::Column::Ordinal)
            .filter(item::Column::ActorId.eq(actor))
            .filter(item::Column::ConversationId.eq(conversation))
            .filter(item::Column::Kind.eq(kind))
            .into_tuple::<(String, i64)>()
            .all(db)
            .await?
            .into_iter()
            .collect();
        let mut next = known.values().max().copied().unwrap_or(0);
        let mut ordinals = Vec::with_capacity(entries.len());
        for entry in entries {
            let item_json = serde_json::to_string(entry).map_err(|_| INVALID)?;
            let item_sha256 = format!("{:x}", Sha256::digest(item_json.as_bytes()));
            let ordinal = match known.get(&item_sha256) {
                Some(ordinal) => *ordinal,
                None => {
                    next = next.checked_add(1).ok_or(AssistantStoreError::Capacity)?;
                    known.insert(item_sha256.clone(), next);
                    new_items.push(item::Model {
                        id: 0,
                        actor_id: actor.clone(),
                        conversation_id: conversation.clone(),
                        kind,
                        ordinal: next,
                        item_sha256,
                        item_json,
                    });
                    next
                }
            };
            ordinals.push(ordinal);
        }
        *list = serde_json::to_value(ranges(&ordinals)).map_err(|_| INVALID)?;
    }
    Ok(EncodedProof {
        proof_json: serde_json::to_string(&value).map_err(|_| INVALID)?,
        parent_summary_json: ParentSummary::from_proof(proof).encode()?,
        new_items,
    })
}

pub async fn insert_items_on<C: ConnectionTrait>(
    db: &C,
    rows: Vec<item::Model>,
) -> Result<(), AssistantStoreError> {
    for row in rows {
        item::Entity::insert(item::ActiveModel {
            actor_id: Set(row.actor_id),
            conversation_id: Set(row.conversation_id),
            kind: Set(row.kind),
            ordinal: Set(row.ordinal),
            item_sha256: Set(row.item_sha256),
            item_json: Set(row.item_json),
            ..Default::default()
        })
        .exec_without_returning(db)
        .await?;
    }
    Ok(())
}

/// Rebuild the proof without validating it; the caller checks its fingerprint.
pub async fn decode_on<C: ConnectionTrait>(
    db: &C,
    actor: &str,
    conversation: &str,
    proof_json: &str,
) -> Result<ArchivedAssistantOutput, AssistantStoreError> {
    let mut value: Value = serde_json::from_str(proof_json).map_err(|_| INVALID)?;
    for (kind, path) in LISTS {
        let list = list_mut(&mut value, path)?;
        let ranges: Vec<(i64, i64)> = serde_json::from_value(list.take()).map_err(|_| INVALID)?;
        let mut entries = Vec::new();
        if let (Some(first), Some(last)) = (
            ranges.iter().map(|range| range.0).min(),
            ranges.iter().map(|range| range.1).max(),
        ) {
            let rows: BTreeMap<i64, String> = item::Entity::find()
                .select_only()
                .column(item::Column::Ordinal)
                .column(item::Column::ItemJson)
                .filter(item::Column::ActorId.eq(actor))
                .filter(item::Column::ConversationId.eq(conversation))
                .filter(item::Column::Kind.eq(kind))
                .filter(item::Column::Ordinal.between(first, last))
                .into_tuple::<(i64, String)>()
                .all(db)
                .await?
                .into_iter()
                .collect();
            for (start, end) in ranges {
                if start > end {
                    return Err(INVALID);
                }
                for ordinal in start..=end {
                    let raw = rows.get(&ordinal).ok_or(INVALID)?;
                    entries.push(serde_json::from_str::<Value>(raw).map_err(|_| INVALID)?);
                }
            }
        }
        *list = Value::Array(entries);
    }
    serde_json::from_value(value).map_err(|_| INVALID)
}

/// Removes every item once no archive in the conversation can reference them.
pub async fn collect_items_on<C: ConnectionTrait>(
    db: &C,
    actor: &str,
    conversation: &str,
) -> Result<i64, AssistantStoreError> {
    let rows = item::Entity::find()
        .filter(item::Column::ActorId.eq(actor))
        .filter(item::Column::ConversationId.eq(conversation))
        .all(db)
        .await?;
    let refund = rows.iter().try_fold(0_i64, |total, row| {
        total
            .checked_add(item_metadata_bytes(row)?)
            .ok_or(AssistantStoreError::Capacity)
    })?;
    item::Entity::delete_many()
        .filter(item::Column::ActorId.eq(actor))
        .filter(item::Column::ConversationId.eq(conversation))
        .exec(db)
        .await?;
    Ok(refund)
}

/// Fields a later call needs from a parent and that no indexed column holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParentSummary {
    pub source_contract_revision: u64,
    pub policy_revision: u64,
    pub retained_policy: Option<SourceUsePolicy>,
    pub retained_roots: Vec<AssistantReadRoot>,
}

impl ParentSummary {
    pub fn from_proof(proof: &ArchivedAssistantOutput) -> Self {
        Self {
            source_contract_revision: proof.source_set.context.source_contract_revision,
            policy_revision: proof.source_set.context.policy_revision,
            retained_policy: proof.retained_policy.clone(),
            retained_roots: proof.retained_roots.clone(),
        }
    }

    fn encode(&self) -> Result<String, AssistantStoreError> {
        serde_json::to_string(self).map_err(|_| INVALID)
    }
}

/// Complete-proof readers call this, so a changed summary is detected there.
pub fn summary_matches(proof: &ArchivedAssistantOutput, raw: &str) -> bool {
    ParentSummary::from_proof(proof)
        .encode()
        .is_ok_and(|expected| expected == raw)
}

/// An accepted parent without its proof body or items.
#[derive(Debug, Clone, PartialEq, Eq, FromQueryResult)]
pub struct ArchiveHead {
    pub actor_id: String,
    pub conversation_id: String,
    pub device_ref: String,
    pub physical_call_id: String,
    pub output_envelope_id: String,
    pub output_digest_sha256: String,
    pub output_label_sha256: String,
    pub actor_input_seq: i64,
    pub input_revision: i64,
    pub completed_at_ms: i64,
    pub parent_summary_json: String,
}

impl ArchiveHead {
    pub fn summary(&self) -> Result<ParentSummary, AssistantStoreError> {
        serde_json::from_str(&self.parent_summary_json).map_err(|_| INVALID)
    }

    /// Equal to `NativeDerivedInputPolicy::from_archive` for the same accepted proof.
    pub fn reference(&self) -> Result<NativeDerivedInputPolicy, AssistantStoreError> {
        let summary = self.summary()?;
        let reference = NativeDerivedInputPolicy {
            actor_id: self.actor_id.clone(),
            conversation_id: self.conversation_id.clone(),
            physical_call_id: self.physical_call_id.clone(),
            output_envelope_id: self.output_envelope_id.clone(),
            output_digest_sha256: self.output_digest_sha256.clone(),
            output_label_sha256: self.output_label_sha256.clone(),
            actor_input_seq: u64::try_from(self.actor_input_seq).map_err(|_| INVALID)?,
            input_revision: u64::try_from(self.input_revision).map_err(|_| INVALID)?,
            source_contract_revision: summary.source_contract_revision,
            policy_revision: summary.policy_revision,
            text_policy: summary.retained_policy,
        };
        reference.validate().map_err(AssistantStoreError::Invalid)?;
        Ok(reference)
    }
}

fn head_query() -> sea_orm::Select<archived::Entity> {
    archived::Entity::find()
        .select_only()
        .column(archived::Column::ActorId)
        .column(archived::Column::ConversationId)
        .column(archived::Column::DeviceRef)
        .column(archived::Column::PhysicalCallId)
        .column(archived::Column::OutputEnvelopeId)
        .column(archived::Column::OutputDigestSha256)
        .column(archived::Column::OutputLabelSha256)
        .column(archived::Column::ActorInputSeq)
        .column(archived::Column::InputRevision)
        .column(archived::Column::CompletedAtMs)
        .column(archived::Column::ParentSummaryJson)
}

/// The output label covers the whole envelope, so a matching head is this output.
pub async fn find_head_on<C: ConnectionTrait>(
    db: &C,
    conversation: &str,
    output: &DataEnvelope,
) -> Result<Option<ArchiveHead>, AssistantStoreError> {
    let rows = head_query()
        .filter(archived::Column::ConversationId.eq(conversation))
        .filter(archived::Column::OutputEnvelopeId.eq(&output.envelope_id))
        .filter(
            archived::Column::OutputDigestSha256.eq(output.content_sha256().unwrap_or_default()),
        )
        .order_by_asc(archived::Column::Id)
        .limit(2)
        .into_model::<ArchiveHead>()
        .all(db)
        .await?;
    if rows.len() > 1 {
        return Err(AssistantStoreError::Inaccessible);
    }
    let Some(head) = rows.into_iter().next() else {
        return Ok(None);
    };
    let label = sink_metadata_sha256(output).map_err(AssistantStoreError::Invalid)?;
    if head.output_label_sha256 != label {
        return Err(AssistantStoreError::Inaccessible);
    }
    Ok(Some(head))
}

pub async fn find_head_by_call_on<C: ConnectionTrait>(
    db: &C,
    actor: &str,
    conversation: &str,
    physical_call_id: &str,
) -> Result<Option<ArchiveHead>, AssistantStoreError> {
    Ok(head_query()
        .filter(archived::Column::ActorId.eq(actor))
        .filter(archived::Column::ConversationId.eq(conversation))
        .filter(archived::Column::PhysicalCallId.eq(physical_call_id))
        .into_model::<ArchiveHead>()
        .one(db)
        .await?)
}
