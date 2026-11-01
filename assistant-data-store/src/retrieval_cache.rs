//! Bounded frozen-query metadata. The caller holds its original actor fence and
//! finishes all native/task/source reads before the final quota mutations here.

use super::{AssistantStoreError, increment, signed, unsigned};
use crate::entity::agent_assistant_retrieval_query as query;
use desk_agent_protocol::ai_assistant::history::ConversationReadRequest;
use desk_agent_protocol::ai_assistant::{
    source::SourceReadPurpose, user_instructions::AssistantDataPolicy,
};
use desk_diagnose_core::ai_assistant::retrieval_state::{
    FrozenRetrievalQuery, FrozenRetrievalRequest, QueryCursor, RetrievalQueryBinding,
};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, sea_query::Expr,
};
#[cfg(test)]
use sea_orm::{DatabaseBackend, QueryOrder, QuerySelect};
use sha2::{Digest, Sha256};

pub struct StoredQuery {
    pub row: query::Model,
    pub proof: Box<FrozenRetrievalQuery>,
}

/// Read the original frozen anchor before trying to resolve a possibly newer
/// observed version. A cursor cannot silently move to another version or chat.
pub async fn load_window_cursor_on<C: ConnectionTrait>(
    db: &C,
    binding: &RetrievalQueryBinding,
    request: &ConversationReadRequest,
    now: u64,
) -> Result<StoredQuery, AssistantStoreError> {
    let raw = request
        .cursor
        .as_deref()
        .ok_or(AssistantStoreError::Inaccessible)?;
    let cursor = QueryCursor::decode(raw).map_err(AssistantStoreError::Invalid)?;
    let stored = load_on(db, &binding.actor_id, &cursor.query_id)
        .await?
        .ok_or(AssistantStoreError::Inaccessible)?;
    validate_window_lookup(&stored, binding, request, now)?;
    stored
        .proof
        .page(raw, binding, &stored.proof.request, now)
        .map_err(AssistantStoreError::Invalid)?;
    Ok(stored)
}

pub async fn load_window_logical_on<C: ConnectionTrait>(
    db: &C,
    binding: &RetrievalQueryBinding,
    logical_call_id: &str,
    request: &ConversationReadRequest,
    now: u64,
) -> Result<Option<StoredQuery>, AssistantStoreError> {
    let id = FrozenRetrievalQuery::query_id(binding, logical_call_id)
        .map_err(AssistantStoreError::Invalid)?;
    let Some(stored) = load_on(db, &binding.actor_id, &id).await? else {
        return Ok(None);
    };
    validate_window_lookup(&stored, binding, request, now)?;
    if stored.proof.initial_logical_call_id != logical_call_id {
        return Err(AssistantStoreError::Inaccessible);
    }
    Ok(Some(stored))
}

fn validate_window_lookup(
    stored: &StoredQuery,
    binding: &RetrievalQueryBinding,
    request: &ConversationReadRequest,
    now: u64,
) -> Result<(), AssistantStoreError> {
    let FrozenRetrievalRequest::Window {
        request: original, ..
    } = &stored.proof.request
    else {
        return Err(AssistantStoreError::Inaccessible);
    };
    if &stored.proof.binding != binding
        || stored.proof.snapshot.purpose != SourceReadPurpose::ModelHistory
        || stored.proof.snapshot.expires_at_unix_ms <= now
        || desk_diagnose_core::ai_assistant::retrieval_query::window_fingerprint(request)
            .map_err(AssistantStoreError::Invalid)?
            != desk_diagnose_core::ai_assistant::retrieval_query::window_fingerprint(original)
                .map_err(AssistantStoreError::Invalid)?
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    Ok(())
}

/// Call before scanning on a logical retry. This lookup never performs another
/// model query, reserves another retrieval call, or renews the original expiry.
pub async fn load_logical_on<C: ConnectionTrait>(
    db: &C,
    binding: &RetrievalQueryBinding,
    logical_call_id: &str,
    request: &FrozenRetrievalRequest,
    now: u64,
) -> Result<Option<StoredQuery>, AssistantStoreError> {
    let id = FrozenRetrievalQuery::query_id(binding, logical_call_id)
        .map_err(AssistantStoreError::Invalid)?;
    let Some(stored) = load_on(db, &binding.actor_id, &id).await? else {
        return Ok(None);
    };
    validate_lookup(&stored, binding, request, now)?;
    if stored.proof.initial_logical_call_id != logical_call_id {
        return Err(AssistantStoreError::Inaccessible);
    }
    Ok(Some(stored))
}

pub async fn load_cursor_on<C: ConnectionTrait>(
    db: &C,
    binding: &RetrievalQueryBinding,
    request: &FrozenRetrievalRequest,
    cursor: &str,
    now: u64,
) -> Result<StoredQuery, AssistantStoreError> {
    let token = QueryCursor::decode(cursor).map_err(AssistantStoreError::Invalid)?;
    let stored = load_on(db, &binding.actor_id, &token.query_id)
        .await?
        .ok_or(AssistantStoreError::Inaccessible)?;
    validate_lookup(&stored, binding, request, now)?;
    stored
        .proof
        .page(cursor, binding, request, now)
        .map_err(AssistantStoreError::Invalid)?;
    Ok(stored)
}

pub async fn prepare_on<C: ConnectionTrait>(
    db: &C,
    proof: &FrozenRetrievalQuery,
    policy: &AssistantDataPolicy,
    now: u64,
) -> Result<StoredQuery, AssistantStoreError> {
    proof.validate().map_err(AssistantStoreError::Invalid)?;
    policy.validate().map_err(AssistantStoreError::Invalid)?;
    if proof.snapshot.policy_revision != policy.revision || proof.snapshot.expires_at_unix_ms <= now
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    if let Some(old) = load_on(db, &proof.snapshot.actor_id, &proof.snapshot.query_id).await? {
        if !old.proof.same_initial_request(proof) {
            return Err(AssistantStoreError::Inaccessible);
        }
        validate_lookup(&old, &proof.binding, &proof.request, now)?;
        return Ok(old);
    }
    let row = to_row(proof, 0, 1)?;
    super::projection::adjust_metadata_on(
        db,
        &row.actor_id,
        metadata_bytes(&row)?,
        Some(policy.max_metadata_bytes),
    )
    .await?;
    let mut active: query::ActiveModel = row.into();
    active.id = sea_orm::ActiveValue::NotSet;
    decode_row(active.insert(db).await?)
}

/// Complete source validation precedes this call. Every error requires rollback
/// of the whole caller transaction, including the byte-counter adjustment.
pub async fn update_on<C: ConnectionTrait>(
    db: &C,
    old: &StoredQuery,
    next: &FrozenRetrievalQuery,
    policy: &AssistantDataPolicy,
    now: u64,
) -> Result<StoredQuery, AssistantStoreError> {
    old.proof
        .validate_successor(next)
        .map_err(AssistantStoreError::Invalid)?;
    policy.validate().map_err(AssistantStoreError::Invalid)?;
    if next.snapshot.policy_revision != policy.revision || next.snapshot.expires_at_unix_ms <= now {
        return Err(AssistantStoreError::Inaccessible);
    }
    // A new SELECT and raw proof comparison are mandatory even inside the same
    // transaction; a prior decode is not a database snapshot authority.
    let fresh = query::Entity::find_by_id(old.row.id)
        .one(db)
        .await?
        .ok_or(AssistantStoreError::Inaccessible)?;
    if fresh != old.row {
        return Err(AssistantStoreError::Inaccessible);
    }
    let row = to_row(next, old.row.id, increment(old.row.revision)?)?;
    if old.proof.as_ref() == next {
        return decode_row(fresh);
    }
    let delta = metadata_bytes(&row)?
        .checked_sub(metadata_bytes(&old.row)?)
        .ok_or(AssistantStoreError::Capacity)?;
    super::projection::adjust_metadata_on(
        db,
        &row.actor_id,
        delta,
        if delta > 0 {
            Some(policy.max_metadata_bytes)
        } else {
            None
        },
    )
    .await?;
    let changed = query::Entity::update_many()
        .col_expr(
            query::Column::ProofJson,
            Expr::value(row.proof_json.clone()),
        )
        .col_expr(
            query::Column::ProofSha256,
            Expr::value(row.proof_sha256.clone()),
        )
        .col_expr(query::Column::Revision, Expr::value(row.revision))
        .filter(query::Column::Id.eq(old.row.id))
        .filter(query::Column::ActorId.eq(&old.row.actor_id))
        .filter(query::Column::Revision.eq(old.row.revision))
        .filter(query::Column::ProofJson.eq(&old.row.proof_json))
        .filter(query::Column::ProofSha256.eq(&old.row.proof_sha256))
        .exec(db)
        .await?;
    if changed.rows_affected != 1 {
        return Err(AssistantStoreError::Inaccessible);
    }
    decode_row(row)
}

/// End-of-transaction metadata cleanup only. Independent read proofs and source
/// evidence have their own lifetimes and are never deleted with this cache.
#[cfg(test)]
pub async fn prune_expired_on<C: ConnectionTrait>(
    db: &C,
    actor: &str,
    now: u64,
    limit: u64,
) -> Result<u64, AssistantStoreError> {
    if limit == 0 || limit > 128 {
        return Err(AssistantStoreError::Invalid("invalid query cleanup batch"));
    }
    let mut select = query::Entity::find()
        .filter(query::Column::ActorId.eq(actor))
        .filter(query::Column::ExpiresAtMs.lte(signed(now)?))
        .order_by_asc(query::Column::ExpiresAtMs)
        .order_by_asc(query::Column::Id)
        .limit(limit);
    if db.get_database_backend() == DatabaseBackend::Postgres {
        select = select.lock_exclusive();
    }
    let rows = select.all(db).await?;
    let mut bytes = 0_i64;
    for row in &rows {
        decode_row(row.clone())?;
        bytes = bytes
            .checked_add(metadata_bytes(row)?)
            .ok_or(AssistantStoreError::Capacity)?;
        let removed = query::Entity::delete_many()
            .filter(query::Column::Id.eq(row.id))
            .filter(query::Column::ActorId.eq(actor))
            .filter(query::Column::Revision.eq(row.revision))
            .filter(query::Column::ProofJson.eq(&row.proof_json))
            .filter(query::Column::ProofSha256.eq(&row.proof_sha256))
            .exec(db)
            .await?;
        if removed.rows_affected != 1 {
            return Err(AssistantStoreError::Inaccessible);
        }
    }
    if bytes > 0 {
        super::projection::refund_existing_metadata_on(db, actor, bytes).await?;
    }
    Ok(rows.len() as u64)
}

async fn load_on<C: ConnectionTrait>(
    db: &C,
    actor: &str,
    id: &str,
) -> Result<Option<StoredQuery>, AssistantStoreError> {
    query::Entity::find()
        .filter(query::Column::ActorId.eq(actor))
        .filter(query::Column::QueryId.eq(id))
        .one(db)
        .await?
        .map(decode_row)
        .transpose()
}

fn validate_lookup(
    stored: &StoredQuery,
    binding: &RetrievalQueryBinding,
    request: &FrozenRetrievalRequest,
    now: u64,
) -> Result<(), AssistantStoreError> {
    if &stored.proof.binding != binding
        || stored.proof.request.kind() != request.kind()
        || stored.proof.snapshot.request_sha256
            != request
                .clone()
                .without_cursor()
                .fingerprint()
                .map_err(AssistantStoreError::Invalid)?
        || stored.proof.snapshot.expires_at_unix_ms <= now
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    Ok(())
}

fn to_row(
    proof: &FrozenRetrievalQuery,
    id: i64,
    revision: i64,
) -> Result<query::Model, AssistantStoreError> {
    let proof_json = proof.encode().map_err(AssistantStoreError::Invalid)?;
    Ok(query::Model {
        id,
        query_id: proof.snapshot.query_id.clone(),
        actor_id: proof.snapshot.actor_id.clone(),
        root_conversation_id: proof.snapshot.root_conversation_id.clone(),
        retrieval_episode_id: proof.snapshot.retrieval_episode_id.clone(),
        purpose: purpose(proof.snapshot.purpose)?.into(),
        kind: proof.request.kind().into(),
        request_sha256: proof.snapshot.request_sha256.clone(),
        scope_sha256: proof
            .binding
            .fingerprint()
            .map_err(AssistantStoreError::Invalid)?,
        proof_sha256: format!("{:x}", Sha256::digest(proof_json.as_bytes())),
        proof_json,
        captured_at_ms: signed(proof.snapshot.captured_at_unix_ms)?,
        expires_at_ms: signed(proof.snapshot.expires_at_unix_ms)?,
        revision,
    })
}

pub fn decode_row(row: query::Model) -> Result<StoredQuery, AssistantStoreError> {
    let proof = Box::new(
        FrozenRetrievalQuery::decode(&row.proof_json).map_err(AssistantStoreError::Invalid)?,
    );
    if row.revision <= 0
        || row.query_id != proof.snapshot.query_id
        || row.actor_id != proof.snapshot.actor_id
        || row.root_conversation_id != proof.snapshot.root_conversation_id
        || row.retrieval_episode_id != proof.snapshot.retrieval_episode_id
        || row.purpose != purpose(proof.snapshot.purpose)?
        || row.kind != proof.request.kind()
        || row.request_sha256 != proof.snapshot.request_sha256
        || row.scope_sha256
            != proof
                .binding
                .fingerprint()
                .map_err(AssistantStoreError::Invalid)?
        || row.proof_json != proof.encode().map_err(AssistantStoreError::Invalid)?
        || row.proof_sha256 != format!("{:x}", Sha256::digest(row.proof_json.as_bytes()))
        || unsigned(row.captured_at_ms)? != proof.snapshot.captured_at_unix_ms
        || unsigned(row.expires_at_ms)? != proof.snapshot.expires_at_unix_ms
    {
        return Err(AssistantStoreError::Invalid("stored query binding changed"));
    }
    Ok(StoredQuery { row, proof })
}

pub fn metadata_bytes(row: &query::Model) -> Result<i64, AssistantStoreError> {
    let fields = [
        &row.query_id,
        &row.actor_id,
        &row.root_conversation_id,
        &row.retrieval_episode_id,
        &row.purpose,
        &row.kind,
        &row.request_sha256,
        &row.scope_sha256,
        &row.proof_sha256,
        &row.proof_json,
    ];
    let bytes = fields
        .into_iter()
        .try_fold(4 * 8_usize, |size, value| size.checked_add(value.len()))
        .ok_or(AssistantStoreError::Capacity)?;
    i64::try_from(bytes).map_err(|_| AssistantStoreError::Capacity)
}

fn purpose(value: SourceReadPurpose) -> Result<&'static str, AssistantStoreError> {
    match value {
        SourceReadPurpose::ModelHistory => Ok("model_history"),
        SourceReadPurpose::ModelMemory => Ok("model_memory"),
        _ => Err(AssistantStoreError::Invalid(
            "unsupported retrieval cache purpose",
        )),
    }
}
