//! One bounded display projection per immutable native answer. No source body or grant.
use super::{AssistantStoreError, signed};
use crate::entity::agent_assistant_citation as citation;
use desk_agent_protocol::ai_assistant::{
    source::{AssistantCitation, validate_source_id},
    user_instructions::AssistantDataPolicy,
};
use desk_diagnose_core::{
    ai_assistant::citation::AnswerCitations,
    native_ids::{self, Kind},
};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, QueryOrder,
    QuerySelect,
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

#[allow(clippy::too_many_arguments)]
pub fn new_row(
    actor: &str,
    conversation: &str,
    answer: &str,
    content: &str,
    output: &str,
    projection: &AnswerCitations,
    recorded: u64,
    expiry: u64,
) -> Result<citation::Model, AssistantStoreError> {
    let row = citation::Model {
        id: 0,
        actor_id: actor.into(),
        answer_message_id: answer.into(),
        answer_revision: 1,
        answer_conversation_id: conversation.into(),
        output_envelope_id: output.into(),
        answer_sha256: format!("{:x}", Sha256::digest(content.as_bytes())),
        citations_json: serde_json::to_string(&projection.citations)
            .map_err(|_| AssistantStoreError::Invalid("invalid citation projection"))?,
        incomplete: projection.incomplete,
        created_at_ms: signed(recorded)?,
        expires_at_ms: signed(expiry)?,
    };
    validate(&row)?;
    Ok(row)
}

fn validate(row: &citation::Model) -> Result<Vec<AssistantCitation>, AssistantStoreError> {
    for id in [
        &row.actor_id,
        &row.answer_conversation_id,
        &row.answer_message_id,
        &row.output_envelope_id,
    ] {
        validate_source_id(id).map_err(AssistantStoreError::Invalid)?;
    }
    if row.answer_revision != 1
        || row.created_at_ms <= 0
        || row.expires_at_ms <= row.created_at_ms
        || row.answer_sha256.len() != 64
        || !row
            .answer_sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    let values: Vec<AssistantCitation> =
        serde_json::from_str(&row.citations_json).map_err(|_| AssistantStoreError::Inaccessible)?;
    let mut ids = BTreeSet::new();
    if values.iter().any(|value| {
        value.answer_message_id != row.answer_message_id
            || value.answer_revision != 1
            || value.source_revision == 0
            || value.source_revision > i64::MAX as u64
            || value.source_device.is_some()
            || !native_ids::is(Kind::AssistantSource, &value.source_id)
            || !ids.insert(value.source_id.as_str())
    }) {
        return Err(AssistantStoreError::Inaccessible);
    }
    Ok(values)
}

fn charge(row: &citation::Model) -> Result<i64, AssistantStoreError> {
    i64::try_from(
        row.actor_id.len()
            + row.answer_conversation_id.len()
            + row.answer_message_id.len()
            + row.output_envelope_id.len()
            + row.answer_sha256.len()
            + row.citations_json.len()
            + 6 * 8,
    )
    .map_err(|_| AssistantStoreError::Capacity)
}

/// The caller owns the original actor fence, completed the native CAS, and
/// uses a savepoint so optional citation storage cannot discard its answer.
pub async fn write_on<C: ConnectionTrait>(
    db: &C,
    actor: &str,
    rows: Vec<citation::Model>,
    policy: &AssistantDataPolicy,
) -> Result<(), AssistantStoreError> {
    let mut bytes = 0_i64;
    for row in &rows {
        validate(row)?;
        if row.actor_id != actor {
            return Err(AssistantStoreError::Inaccessible);
        }
        bytes = bytes
            .checked_add(charge(row)?)
            .ok_or(AssistantStoreError::Capacity)?;
    }
    if bytes == 0 {
        return Ok(());
    }
    super::projection::adjust_metadata_on(db, actor, bytes, Some(policy.max_metadata_bytes))
        .await?;
    // Only own display metadata is written after its quota phase.
    for row in rows {
        let mut row: citation::ActiveModel = row.into();
        row.id = sea_orm::ActiveValue::NotSet;
        row.insert(db).await?;
    }
    Ok(())
}

/// Owner snapshot supplies the actual page's immutable answer IDs and text.
/// Current source access is resolved on click, never granted by this projection.
pub async fn load_on<C: ConnectionTrait>(
    db: &C,
    actor: &str,
    conversation: &str,
    answers: &[(String, String)],
    now: u64,
    batch_size: u32,
) -> Result<BTreeMap<String, AnswerCitations>, AssistantStoreError> {
    if answers.is_empty() {
        return Ok(BTreeMap::new());
    }
    let mut result = BTreeMap::new();
    for answers in answers.chunks(batch_size as usize) {
        let rows = citation::Entity::find()
            .filter(citation::Column::ActorId.eq(actor))
            .filter(citation::Column::AnswerConversationId.eq(conversation))
            .filter(
                citation::Column::AnswerMessageId.is_in(answers.iter().map(|(id, _)| id.clone())),
            )
            .filter(citation::Column::ExpiresAtMs.gt(signed(now)?))
            .limit(answers.len() as u64 + 1)
            .all(db)
            .await?;
        for row in rows {
            let Some((_, text)) = answers.iter().find(|(id, _)| *id == row.answer_message_id)
            else {
                return Err(AssistantStoreError::Inaccessible);
            };
            if row.answer_sha256 != format!("{:x}", Sha256::digest(text.as_bytes())) {
                continue;
            }
            let Ok(values) = validate(&row) else {
                continue;
            };
            if result
                .insert(
                    row.answer_message_id,
                    AnswerCitations {
                        citations: values,
                        incomplete: row.incomplete,
                    },
                )
                .is_some()
            {
                return Err(AssistantStoreError::Inaccessible);
            }
        }
    }
    Ok(result)
}

pub struct CitationGc {
    pub refunded_bytes: i64,
    pub has_more: bool,
    pub invalid: u64,
}

/// No native/source reads follow the caller's final combined quota refund.
/// Rows are immutable; refund only exactly fenced display projections removed here.
pub async fn collect_expired_on<C: ConnectionTrait>(
    db: &C,
    actor: &str,
    now: i64,
    maximum: u64,
) -> Result<CitationGc, AssistantStoreError> {
    let rows = citation::Entity::find()
        .filter(citation::Column::ActorId.eq(actor))
        .filter(citation::Column::ExpiresAtMs.lte(now))
        .order_by_asc(citation::Column::Id)
        .limit(maximum)
        .all(db)
        .await?;
    let mut plan = CitationGc {
        refunded_bytes: 0,
        has_more: rows.len() as u64 == maximum,
        invalid: 0,
    };
    for row in rows {
        if validate(&row).is_err() {
            plan.invalid += 1;
            continue;
        }
        let bytes = charge(&row)?;
        let deleted = citation::Entity::delete_many()
            .filter(citation::Column::Id.eq(row.id))
            .filter(citation::Column::ActorId.eq(actor))
            .filter(citation::Column::AnswerConversationId.eq(&row.answer_conversation_id))
            .filter(citation::Column::AnswerMessageId.eq(&row.answer_message_id))
            .filter(citation::Column::AnswerRevision.eq(row.answer_revision))
            .filter(citation::Column::OutputEnvelopeId.eq(&row.output_envelope_id))
            .filter(citation::Column::AnswerSha256.eq(&row.answer_sha256))
            .filter(citation::Column::CitationsJson.eq(&row.citations_json))
            .filter(citation::Column::Incomplete.eq(row.incomplete))
            .filter(citation::Column::CreatedAtMs.eq(row.created_at_ms))
            .filter(citation::Column::ExpiresAtMs.eq(row.expires_at_ms))
            .exec(db)
            .await?;
        if deleted.rows_affected != 1 {
            return Err(AssistantStoreError::Inaccessible);
        }
        plan.refunded_bytes = plan
            .refunded_bytes
            .checked_add(bytes)
            .ok_or(AssistantStoreError::Capacity)?;
    }
    // Corrupt optional metadata remains held and observable without an
    // immediate rescan loop. It never enables a citation or earns a refund.
    plan.has_more &= plan.invalid == 0;
    Ok(plan)
}
