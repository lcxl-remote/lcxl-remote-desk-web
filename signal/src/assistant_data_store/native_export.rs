//! Actual-use ledger. The caller retains original actor/task/session fences and
//! commits these rows only with the native provider-start transaction.

use super::{
    AssistantStoreError, increment,
    projection::{StoredProjection, adjust_metadata_on, load_on as load_projection_on},
    signed, unsigned,
};
use crate::entity::{
    agent_assistant_data_authorization as authorization,
    agent_assistant_read_projection as projection, agent_assistant_source_use as source_use,
    agent_memory_use as memory_use,
};
use desk_agent_protocol::ai_assistant::{
    memory::MemoryUsePurpose, source::SourceReadPurpose, user_instructions::AssistantDataPolicy,
};
use desk_diagnose_core::ai_assistant::native_export::{
    AssistantDataExportAuthorization, AssistantNativeSourceSet, input_lineage_sha256,
};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, QuerySelect,
    sea_query::Expr,
};

mod owner_history;
pub(crate) use owner_history::load_owner_history_set_on;

#[derive(PartialEq, Eq)]
pub(crate) struct StoredAuthorization {
    pub row: authorization::Model,
    pub proof: Box<AssistantDataExportAuthorization>,
}

struct NewAuthorization {
    source: StoredProjection,
    row: authorization::Model,
    sources: Vec<source_use::Model>,
    memories: Vec<memory_use::Model>,
    charge: i64,
}
enum PreparedAuthorization {
    Existing(Box<StoredAuthorization>),
    New(Box<NewAuthorization>),
}

/// Validate every original native/projection/use binding before any quota write.
/// A physical call is sealed as a complete batch, never a partially charged set.
#[cfg(test)]
pub(crate) async fn record_call_on<C: ConnectionTrait>(
    db: &C,
    grants: &[AssistantDataExportAuthorization],
    policy: &AssistantDataPolicy,
) -> Result<Vec<StoredAuthorization>, AssistantStoreError> {
    let set =
        AssistantNativeSourceSet::from_grants(grants).map_err(AssistantStoreError::Invalid)?;
    record_sealed_call_on(db, &set, grants, policy).await
}

pub(crate) async fn record_sealed_call_on<C: ConnectionTrait>(
    db: &C,
    set: &AssistantNativeSourceSet,
    grants: &[AssistantDataExportAuthorization],
    policy: &AssistantDataPolicy,
) -> Result<Vec<StoredAuthorization>, AssistantStoreError> {
    policy.validate().map_err(AssistantStoreError::Invalid)?;
    set.validate_grants(grants)
        .map_err(AssistantStoreError::Invalid)?;
    if set.context.policy_revision != policy.revision {
        return Err(AssistantStoreError::Inaccessible);
    }
    let mut prepared = Vec::with_capacity(grants.len());
    let mut charge = 0_i64;
    for grant in grants {
        let item =
            desk_diagnose_core::future::boxed(|| prepare_grant_on(db, grant, policy)).await?;
        if let PreparedAuthorization::New(item) = &item {
            charge = charge
                .checked_add(item.charge)
                .ok_or(AssistantStoreError::Capacity)?;
        }
        prepared.push(item);
    }
    let replay = !prepared.is_empty()
        && prepared
            .iter()
            .all(|item| matches!(item, PreparedAuthorization::Existing(_)));
    if !replay
        && prepared
            .iter()
            .any(|item| matches!(item, PreparedAuthorization::Existing(_)))
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    let native_charge = prepare_call_set_on(db, grants, set, replay).await?;
    charge = charge
        .checked_add(native_charge)
        .ok_or(AssistantStoreError::Capacity)?;
    for (item, grant) in prepared.iter().zip(grants) {
        if let PreparedAuthorization::New(item) = item {
            consume_projection_on(db, &item.source, grant.authorized_at_unix_ms).await?;
        }
    }
    // Native sealing and all source/projection reads and consumption precede the
    // single quota mutation. Only authorization/use metadata is inserted below.
    if charge > 0 {
        adjust_metadata_on(db, &set.actor_id, charge, Some(policy.max_metadata_bytes)).await?;
    }
    let mut stored = Vec::with_capacity(prepared.len());
    for item in prepared {
        match item {
            PreparedAuthorization::Existing(value) => stored.push(*value),
            PreparedAuthorization::New(item) => {
                let NewAuthorization {
                    source: _,
                    row,
                    sources,
                    memories,
                    charge: _,
                } = *item;
                let mut active: authorization::ActiveModel = row.into();
                active.id = sea_orm::ActiveValue::NotSet;
                let accepted = active.insert(db).await?;
                for usage in sources {
                    let mut active: source_use::ActiveModel = usage.into();
                    active.id = sea_orm::ActiveValue::NotSet;
                    active.insert(db).await?;
                }
                for usage in memories {
                    let mut active: memory_use::ActiveModel = usage.into();
                    active.id = sea_orm::ActiveValue::NotSet;
                    active.insert(db).await?;
                }
                stored.push(decode_row(accepted)?);
            }
        }
    }
    Ok(stored)
}

async fn prepare_grant_on<C: ConnectionTrait>(
    db: &C,
    grant: &AssistantDataExportAuthorization,
    policy: &AssistantDataPolicy,
) -> Result<PreparedAuthorization, AssistantStoreError> {
    grant.validate().map_err(AssistantStoreError::Invalid)?;
    if grant.policy_revision != policy.revision {
        return Err(AssistantStoreError::Inaccessible);
    }
    verify_native_on(db, grant).await?;
    let source = load_projection_on(db, &grant.actor_id, &grant.projection_id)
        .await?
        .ok_or(AssistantStoreError::Inaccessible)?;
    grant
        .validate_projection(&source.proof)
        .map_err(AssistantStoreError::Invalid)?;
    if source.row.revoked_at_ms.is_some()
        || !source.proof.preparation_available(
            grant.authorized_at_unix_ms,
            source.row.first_consumed_at_ms.map(unsigned).transpose()?,
        )
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    if let Some(old) = load_on(db, &grant.actor_id, &grant.authorization_id).await? {
        if *old.proof != *grant
            || old.row.revoked_at_ms.is_some()
            || source.row.first_consumed_at_ms.is_none()
        {
            return Err(AssistantStoreError::Inaccessible);
        }
        verify_use_rows_on(db, grant, &source).await?;
        return Ok(PreparedAuthorization::Existing(Box::new(old)));
    }
    let row = authorization::Model {
        id: 0,
        authorization_id: grant.authorization_id.clone(),
        actor_id: grant.actor_id.clone(),
        physical_call_id: grant.native_call.physical_call_id.clone(),
        consumer_conversation_id: grant.consumer_conversation_id.clone(),
        projection_id: grant.projection_id.clone(),
        projection_sha256: grant.projection_sha256.clone(),
        purpose: purpose(grant.purpose).into(),
        proof_json: serde_json::to_string(grant)
            .map_err(|_| AssistantStoreError::Invalid("invalid assistant export proof"))?,
        authorized_at_ms: signed(grant.authorized_at_unix_ms)?,
        expires_at_ms: signed(grant.expires_at_unix_ms)?,
        revoked_at_ms: None,
    };
    let (sources, memories) = use_rows(grant, &source)?;
    let mut charge = metadata_bytes(&row)?;
    for usage in &sources {
        charge = charge
            .checked_add(source_use_bytes(usage)?)
            .ok_or(AssistantStoreError::Capacity)?;
    }
    for usage in &memories {
        charge = charge
            .checked_add(memory_use_bytes(usage)?)
            .ok_or(AssistantStoreError::Capacity)?;
    }
    Ok(PreparedAuthorization::New(Box::new(NewAuthorization {
        source,
        row,
        sources,
        memories,
        charge,
    })))
}

#[cfg(test)]
pub(crate) async fn record_on<C: ConnectionTrait>(
    db: &C,
    grant: &AssistantDataExportAuthorization,
    policy: &AssistantDataPolicy,
) -> Result<StoredAuthorization, AssistantStoreError> {
    record_call_on(db, std::slice::from_ref(grant), policy)
        .await?
        .pop()
        .ok_or(AssistantStoreError::Inaccessible)
}

pub(crate) async fn load_on<C: ConnectionTrait>(
    db: &C,
    actor: &str,
    authorization_id: &str,
) -> Result<Option<StoredAuthorization>, AssistantStoreError> {
    authorization::Entity::find()
        .filter(authorization::Column::ActorId.eq(actor))
        .filter(authorization::Column::AuthorizationId.eq(authorization_id))
        .one(db)
        .await?
        .map(decode_row)
        .transpose()
}

async fn prepare_call_set_on<C: ConnectionTrait>(
    db: &C,
    grants: &[AssistantDataExportAuthorization],
    set: &AssistantNativeSourceSet,
    replay: bool,
) -> Result<i64, AssistantStoreError> {
    verify_native_set_on(db, set).await?;
    let encoded = serde_json::to_string(set)
        .map_err(|_| AssistantStoreError::Invalid("invalid native source-set encoding"))?;
    use crate::entity::model_egress_receipt as native;
    let row = native::Entity::find_by_id(&set.physical_call_id)
        .one(db)
        .await?
        .ok_or(AssistantStoreError::Inaccessible)?;
    if let Some(existing) = &row.assistant_source_set_json {
        if existing != &encoded || (!replay && !grants.is_empty()) {
            return Err(AssistantStoreError::Inaccessible);
        }
        load_call_set_on(db, set).await?;
        return Ok(0);
    }
    if replay {
        return Err(AssistantStoreError::Inaccessible);
    }
    // Keep the original complete-call authorization SELECT. A new seal cannot
    // silently absorb orphan or partially committed grants from another state.
    let existing = authorization::Entity::find()
        .filter(authorization::Column::PhysicalCallId.eq(&set.physical_call_id))
        .limit(33)
        .all(db)
        .await?;
    if !existing.is_empty() {
        return Err(AssistantStoreError::Inaccessible);
    }
    let charge = i64::try_from(encoded.len()).map_err(|_| AssistantStoreError::Capacity)?;
    let changed = native::Entity::update_many()
        .col_expr(
            native::Column::AssistantSourceSetJson,
            Expr::value(Some(encoded)),
        )
        .filter(native::Column::ReceiptId.eq(&set.physical_call_id))
        .filter(native::Column::AssistantSourceSetJson.is_null())
        .filter(native::Column::State.eq(crate::model_egress_store::STATE_DISPATCH_INTENT))
        .filter(native::Column::CompletedAt.is_null())
        .filter(native::Column::ExportAuthorizationId.eq(row.export_authorization_id))
        .filter(native::Column::AuthorizedAt.eq(row.authorized_at))
        .filter(
            native::Column::ContextConversationId.eq(Some(set.consumer_conversation_id.clone())),
        )
        .filter(native::Column::DestinationJson.eq(row.destination_json))
        .filter(native::Column::EnvelopeIdsJson.eq(row.envelope_ids_json))
        .filter(native::Column::DigestsSha256Json.eq(row.digests_sha256_json))
        .filter(native::Column::InputLineageJson.eq(row.input_lineage_json))
        .filter(native::Column::InputSnapshotJson.eq(row.input_snapshot_json))
        .filter(native::Column::ProjectionDigestSha256.eq(row.projection_digest_sha256))
        .filter(native::Column::TotalBytes.eq(row.total_bytes))
        .exec(db)
        .await?;
    if changed.rows_affected != 1 {
        return Err(AssistantStoreError::Inaccessible);
    }
    Ok(charge)
}

/// Validate the exact complete reverse-use set without applying the short send
/// TTL to a provider response that was already in flight.
pub(crate) async fn load_call_set_on<C: ConnectionTrait>(
    db: &C,
    set: &AssistantNativeSourceSet,
) -> Result<Vec<StoredAuthorization>, AssistantStoreError> {
    Ok(load_call_set_rows_on(db, set, CallSetUse::Current)
        .await?
        .into_iter()
        .map(|(authorization, _)| authorization)
        .collect())
}

/// Keep the authenticated read rows for callers that will recheck this exact
/// native batch, instead of discarding and decoding each projection again.
pub(crate) async fn load_call_set_with_projections_on<C: ConnectionTrait>(
    db: &C,
    set: &AssistantNativeSourceSet,
) -> Result<Vec<(StoredAuthorization, StoredProjection)>, AssistantStoreError> {
    load_call_set_rows_on(db, set, CallSetUse::Current).await
}

/// A control finisher may inspect only its exact original grant rows after its
/// own revocation. The returned metadata never permits another provider send.
/// The caller retains the native fence and its complete owned-batch checks.
pub(crate) async fn load_call_set_before_control_on<C: ConnectionTrait>(
    db: &C,
    set: &AssistantNativeSourceSet,
    control: &super::memory_control::PendingControl,
    originals: &[authorization::Model],
) -> Result<Vec<StoredAuthorization>, AssistantStoreError> {
    Ok(
        load_call_set_rows_on(db, set, CallSetUse::BeforeControl(control, originals))
            .await?
            .into_iter()
            .map(|(authorization, _)| authorization)
            .collect(),
    )
}

/// The original accepted consumption, with no cached result body or new grant.
/// Its callers authenticate the native output and retain their session fence.
#[derive(PartialEq, Eq)]
pub(crate) struct ConsumedReadMetadata {
    pub authorization: StoredAuthorization,
    pub projection: Box<desk_diagnose_core::ai_assistant::read_projection::ReadProjectionProof>,
    pub projection_revoked_at_ms: Option<i64>,
    pub first_consumed_at_unix_ms: u64,
    pub last_consumed_at_unix_ms: u64,
}

#[derive(Clone, Copy)]
enum CallSetUse<'a> {
    Current,
    BeforeControl(
        &'a super::memory_control::PendingControl,
        &'a [authorization::Model],
    ),
    ConsumedMetadata,
}

/// Only committed consumption metadata may survive purpose revocation. Source
/// body reads and physical model dispatch must still use their current gates.
pub(crate) async fn load_consumed_metadata_on<C: ConnectionTrait>(
    db: &C,
    set: &AssistantNativeSourceSet,
) -> Result<Vec<ConsumedReadMetadata>, AssistantStoreError> {
    desk_diagnose_core::future::send_phase(|| async {
        load_call_set_rows_on(db, set, CallSetUse::ConsumedMetadata)
            .await?
            .into_iter()
            .map(|(authorization, source)| {
                Ok(ConsumedReadMetadata {
                    authorization,
                    projection: source.proof,
                    projection_revoked_at_ms: source.row.revoked_at_ms,
                    first_consumed_at_unix_ms: unsigned(
                        source
                            .row
                            .first_consumed_at_ms
                            .ok_or(AssistantStoreError::Inaccessible)?,
                    )?,
                    last_consumed_at_unix_ms: unsigned(
                        source
                            .row
                            .last_consumed_at_ms
                            .ok_or(AssistantStoreError::Inaccessible)?,
                    )?,
                })
            })
            .collect()
    })
    .await
}

async fn load_call_set_rows_on<C: ConnectionTrait>(
    db: &C,
    set: &AssistantNativeSourceSet,
    purpose: CallSetUse<'_>,
) -> Result<Vec<(StoredAuthorization, StoredProjection)>, AssistantStoreError> {
    set.validate().map_err(AssistantStoreError::Invalid)?;
    let rows = authorization::Entity::find()
        .filter(authorization::Column::PhysicalCallId.eq(&set.physical_call_id))
        .limit(33)
        .all(db)
        .await?;
    let mut stored = Vec::with_capacity(rows.len());
    for row in rows {
        let mut value = decode_row(row)?;
        let source = load_projection_on(db, &set.actor_id, &value.proof.projection_id)
            .await?
            .ok_or(AssistantStoreError::Inaccessible)?;
        if let CallSetUse::BeforeControl(control, originals) = &purpose {
            let before = originals
                .iter()
                .find(|row| row.id == value.row.id)
                .ok_or(AssistantStoreError::Inaccessible)?;
            let mut expected = before.clone();
            if control.revokes_usage(&source.proof.usage) {
                expected.revoked_at_ms = Some(control.revocation_time());
            }
            if value.row != expected {
                return Err(AssistantStoreError::Inaccessible);
            }
            value.row = before.clone();
        }
        if let Some(revoked) = value.row.revoked_at_ms {
            if !matches!(purpose, CallSetUse::ConsumedMetadata) {
                return Err(AssistantStoreError::Inaccessible);
            }
            unsigned(revoked)?;
        }
        value
            .proof
            .validate_projection(&source.proof)
            .map_err(AssistantStoreError::Invalid)?;
        if !matches!(purpose, CallSetUse::ConsumedMetadata) && source.row.revoked_at_ms.is_some()
            || source
                .row
                .first_consumed_at_ms
                .is_none_or(|at| at > value.row.authorized_at_ms)
            || source
                .row
                .last_consumed_at_ms
                .is_none_or(|at| at < value.row.authorized_at_ms)
        {
            return Err(AssistantStoreError::Inaccessible);
        }
        verify_use_rows_on(db, &value.proof, &source).await?;
        if let Some(revoked) = source.row.revoked_at_ms {
            unsigned(revoked)?;
        }
        stored.push((value, source));
    }
    let proofs = stored
        .iter()
        .map(|(authorization, _)| authorization.proof.as_ref().clone())
        .collect::<Vec<_>>();
    set.validate_grants(&proofs)
        .map_err(AssistantStoreError::Invalid)?;
    Ok(stored)
}

pub(crate) fn decode_row(
    row: authorization::Model,
) -> Result<StoredAuthorization, AssistantStoreError> {
    let proof: Box<AssistantDataExportAuthorization> = serde_json::from_str(&row.proof_json)
        .map_err(|_| {
            AssistantStoreError::Invalid("invalid stored assistant physical-call grant")
        })?;
    proof.validate().map_err(AssistantStoreError::Invalid)?;
    if row.authorization_id != proof.authorization_id
        || row.actor_id != proof.actor_id
        || row.physical_call_id != proof.native_call.physical_call_id
        || row.consumer_conversation_id != proof.consumer_conversation_id
        || row.projection_id != proof.projection_id
        || row.projection_sha256 != proof.projection_sha256
        || row.purpose != purpose(proof.purpose)
        || unsigned(row.authorized_at_ms)? != proof.authorized_at_unix_ms
        || unsigned(row.expires_at_ms)? != proof.expires_at_unix_ms
    {
        return Err(AssistantStoreError::Invalid(
            "stored assistant physical-call grant binding changed",
        ));
    }
    Ok(StoredAuthorization { row, proof })
}

pub(crate) async fn consume_projection_on<C: ConnectionTrait>(
    db: &C,
    source: &StoredProjection,
    now: u64,
) -> Result<(), AssistantStoreError> {
    let first = source.row.first_consumed_at_ms.unwrap_or(signed(now)?);
    let last = source
        .row
        .last_consumed_at_ms
        .map_or(signed(now)?, |at| at.max(now as i64));
    let mut update = projection::Entity::update_many()
        .col_expr(
            projection::Column::FirstConsumedAtMs,
            Expr::value(Some(first)),
        )
        .col_expr(
            projection::Column::LastConsumedAtMs,
            Expr::value(Some(last)),
        )
        .col_expr(
            projection::Column::Revision,
            Expr::value(increment(source.row.revision)?),
        )
        .filter(projection::Column::Id.eq(source.row.id))
        .filter(projection::Column::ActorId.eq(&source.row.actor_id))
        .filter(projection::Column::Revision.eq(source.row.revision))
        .filter(projection::Column::RevokedAtMs.is_null())
        .filter(projection::Column::ProofJson.eq(&source.row.proof_json))
        .filter(projection::Column::PayloadSha256.eq(&source.row.payload_sha256));
    update = if let Some(body) = &source.row.result_json {
        update.filter(projection::Column::ResultJson.eq(body))
    } else {
        update.filter(projection::Column::ResultJson.is_null())
    };
    update = if let Some(first) = source.row.first_consumed_at_ms {
        update.filter(projection::Column::FirstConsumedAtMs.eq(first))
    } else {
        update.filter(projection::Column::FirstConsumedAtMs.is_null())
    };
    update = if let Some(last) = source.row.last_consumed_at_ms {
        update.filter(projection::Column::LastConsumedAtMs.eq(last))
    } else {
        update.filter(projection::Column::LastConsumedAtMs.is_null())
    };
    let changed = update.exec(db).await?;
    if changed.rows_affected != 1 {
        return Err(AssistantStoreError::Inaccessible);
    }
    Ok(())
}

fn use_rows(
    grant: &AssistantDataExportAuthorization,
    source: &StoredProjection,
) -> Result<(Vec<source_use::Model>, Vec<memory_use::Model>), AssistantStoreError> {
    let created = signed(grant.authorized_at_unix_ms)?;
    // A short physical-send deadline is not the lifetime of derived context.
    // These reverse references retain the original hard source deadline only.
    let expiry = source
        .proof
        .usage
        .expires_at_unix_ms
        .map(signed)
        .transpose()?;
    let sources = source
        .proof
        .usage
        .history_dependencies
        .iter()
        .map(|dependency| {
            Ok(source_use::Model {
                id: 0,
                actor_id: grant.actor_id.clone(),
                physical_call_id: grant.native_call.physical_call_id.clone(),
                consumer_conversation_id: grant.consumer_conversation_id.clone(),
                source_id: dependency.source_id.clone(),
                source_revision: signed(dependency.source_revision)?,
                purpose: purpose(grant.purpose).into(),
                authorization_id: grant.authorization_id.clone(),
                created_at_ms: created,
                expires_at_ms: expiry,
            })
        })
        .collect::<Result<Vec<_>, AssistantStoreError>>()?;
    let memories = source
        .proof
        .usage
        .memories
        .iter()
        .map(|memory| {
            Ok(memory_use::Model {
                id: 0,
                actor_id: grant.actor_id.clone(),
                physical_call_id: grant.native_call.physical_call_id.clone(),
                consumer_conversation_id: grant.consumer_conversation_id.clone(),
                memory_id: memory.memory_id.clone(),
                memory_revision: signed(memory.revision)?,
                purpose: memory_purpose(memory.purpose).into(),
                authorization_id: grant.authorization_id.clone(),
                created_at_ms: created,
                expires_at_ms: expiry,
            })
        })
        .collect::<Result<Vec<_>, AssistantStoreError>>()?;
    Ok((sources, memories))
}

async fn verify_use_rows_on<C: ConnectionTrait>(
    db: &C,
    grant: &AssistantDataExportAuthorization,
    source: &StoredProjection,
) -> Result<(), AssistantStoreError> {
    let (mut expected_sources, mut expected_memories) = use_rows(grant, source)?;
    let mut sources = source_use::Entity::find()
        .filter(source_use::Column::ActorId.eq(&grant.actor_id))
        .filter(source_use::Column::AuthorizationId.eq(&grant.authorization_id))
        .limit(129)
        .all(db)
        .await?;
    let mut memories = memory_use::Entity::find()
        .filter(memory_use::Column::ActorId.eq(&grant.actor_id))
        .filter(memory_use::Column::AuthorizationId.eq(&grant.authorization_id))
        .limit(49)
        .all(db)
        .await?;
    for row in &mut sources {
        row.id = 0;
    }
    for row in &mut memories {
        row.id = 0;
    }
    sources
        .sort_by(|a, b| (&a.source_id, a.source_revision).cmp(&(&b.source_id, b.source_revision)));
    expected_sources
        .sort_by(|a, b| (&a.source_id, a.source_revision).cmp(&(&b.source_id, b.source_revision)));
    memories.sort_by(|a, b| {
        (&a.memory_id, a.memory_revision, &a.purpose).cmp(&(
            &b.memory_id,
            b.memory_revision,
            &b.purpose,
        ))
    });
    expected_memories.sort_by(|a, b| {
        (&a.memory_id, a.memory_revision, &a.purpose).cmp(&(
            &b.memory_id,
            b.memory_revision,
            &b.purpose,
        ))
    });
    if sources != expected_sources || memories != expected_memories {
        return Err(AssistantStoreError::Inaccessible);
    }
    Ok(())
}

pub(crate) fn metadata_bytes(row: &authorization::Model) -> Result<i64, AssistantStoreError> {
    count_bytes(
        4,
        &[
            &row.authorization_id,
            &row.actor_id,
            &row.physical_call_id,
            &row.consumer_conversation_id,
            &row.projection_id,
            &row.projection_sha256,
            &row.purpose,
            &row.proof_json,
        ],
    )
}

pub(crate) fn source_use_bytes(row: &source_use::Model) -> Result<i64, AssistantStoreError> {
    count_bytes(
        4,
        &[
            &row.actor_id,
            &row.physical_call_id,
            &row.consumer_conversation_id,
            &row.source_id,
            &row.purpose,
            &row.authorization_id,
        ],
    )
}

pub(crate) fn memory_use_bytes(row: &memory_use::Model) -> Result<i64, AssistantStoreError> {
    count_bytes(
        4,
        &[
            &row.actor_id,
            &row.physical_call_id,
            &row.consumer_conversation_id,
            &row.memory_id,
            &row.purpose,
            &row.authorization_id,
        ],
    )
}

fn count_bytes(scalars: usize, strings: &[&str]) -> Result<i64, AssistantStoreError> {
    let bytes = strings
        .iter()
        .try_fold(scalars * 8, |sum, field| sum.checked_add(field.len()))
        .ok_or(AssistantStoreError::Invalid(
            "assistant authorization size overflow",
        ))?;
    i64::try_from(bytes)
        .map_err(|_| AssistantStoreError::Invalid("assistant authorization size overflow"))
}

fn purpose(value: SourceReadPurpose) -> &'static str {
    match value {
        SourceReadPurpose::OwnerHistory => "owner_history",
        SourceReadPurpose::ModelHistory => "model_history",
        SourceReadPurpose::MemoryEvidence => "memory_evidence",
        SourceReadPurpose::ModelMemory => "model_memory",
    }
}

fn memory_purpose(value: MemoryUsePurpose) -> &'static str {
    match value {
        MemoryUsePurpose::CurrentFact => "current_fact",
        MemoryUsePurpose::HistoricalReference => "historical_reference",
    }
}

async fn verify_native_set_on<C: ConnectionTrait>(
    db: &C,
    set: &AssistantNativeSourceSet,
) -> Result<(), AssistantStoreError> {
    use crate::entity::model_egress_receipt as native;
    let row = native::Entity::find_by_id(&set.physical_call_id)
        .one(db)
        .await?
        .ok_or(AssistantStoreError::Inaccessible)?;
    if row.state != crate::model_egress_store::STATE_DISPATCH_INTENT
        || row.completed_at.is_some()
        || row.context_conversation_id.as_deref() != Some(set.consumer_conversation_id.as_str())
        || unsigned(row.authorized_at.timestamp_millis())? != set.context.authorized_at_unix_ms
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    let (audit, inputs, _) = crate::model_egress_store::receipt_inputs(&row)?;
    if row.export_authorization_id != set.model_export_authorization_id
        || audit.destination != set.destination
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    set.validate_input_lineage(&inputs)
        .map_err(AssistantStoreError::Invalid)
}

async fn verify_native_on<C: ConnectionTrait>(
    db: &C,
    grant: &AssistantDataExportAuthorization,
) -> Result<(), AssistantStoreError> {
    use crate::entity::model_egress_receipt as native;
    let row = native::Entity::find_by_id(&grant.native_call.physical_call_id)
        .one(db)
        .await?
        .ok_or(AssistantStoreError::Inaccessible)?;
    if row.state != crate::model_egress_store::STATE_DISPATCH_INTENT
        || row.completed_at.is_some()
        || row.context_conversation_id.as_deref() != Some(grant.consumer_conversation_id.as_str())
        || unsigned(row.authorized_at.timestamp_millis())? != grant.authorized_at_unix_ms
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    let (audit, inputs, _) = crate::model_egress_store::receipt_inputs(&row)?;
    if row.export_authorization_id != grant.native_call.model_export_authorization_id
        || audit.destination != grant.native_call.destination
        || input_lineage_sha256(&inputs).map_err(AssistantStoreError::Invalid)?
            != grant.native_call.input_lineage_sha256
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    Ok(())
}
