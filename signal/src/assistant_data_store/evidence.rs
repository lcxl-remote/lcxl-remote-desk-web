//! Independent evidence validation is shared by owner viewing and model readers.
//! It preserves source constraints but does not itself authorize an export.

use super::{AssistantStoreError, unsigned};
use crate::entity::{
    agent_assistant_origin as origin, agent_memory_revision as revision,
    agent_memory_source as evidence,
};
use desk_agent_protocol::ai_assistant::source::{
    AssistantSourceKind, HistoryDependency, SourceRefRole, SourceUsePolicy,
};
use desk_diagnose_core::ai_assistant::{
    memory_control::KnownMemorySource,
    memory_evidence::{
        MemoryEvidenceRetentionProof, MemoryEvidenceSnapshot, RetainedMemoryOrigin,
        canonical_retained_origins,
    },
    source::validate_policy,
};
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter};

/// Only authenticated link identities can expand source suppression during
/// owner deletion. Body corruption must not require retaining the fact itself.
pub(crate) use desk_assistant_data_store::evidence::forgetting_sources;

pub(crate) async fn allowed_on<C: ConnectionTrait + crate::config::ConfigConnection>(
    db: &C,
    actor: &str,
    row: &evidence::Model,
    now: u64,
) -> Result<bool, AssistantStoreError> {
    desk_diagnose_core::future::boxed(|| {
        validate_on(db, actor, row, EvidenceReadMode::Body, now, None)
    })
    .await
    .map(|value| value.is_some())
}

/// Only original owner history viewing uses this path. It may validate an
/// already retained original-chat copy after memory forgetting, never a model
/// export or a read of the forgotten memory's own body.
pub(crate) async fn owner_history_identity_allowed_on<
    C: ConnectionTrait + crate::config::ConfigConnection,
>(
    db: &C,
    actor: &str,
    row: &evidence::Model,
    now: u64,
) -> Result<bool, AssistantStoreError> {
    desk_diagnose_core::future::boxed(|| {
        validate_on(
            db,
            actor,
            row,
            EvidenceReadMode::OwnerHistoryIdentity,
            now,
            None,
        )
    })
    .await
    .map(|value| value.is_some())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EvidenceReadMode {
    Body,
    RetainedIdentity,
    OwnerHistoryIdentity,
}

pub(crate) struct DecodedEvidence {
    pub policy: SourceUsePolicy,
    pub primary: origin::Model,
    pub primary_policy: SourceUsePolicy,
    pub forget: Vec<KnownMemorySource>,
    pub known: Vec<KnownMemorySource>,
    pub dependencies: Vec<HistoryDependency>,
}

/// Return the already checked metadata to the source reader. The owner fence
/// and all current source/revocation queries remain inside the validation.
pub(crate) async fn load_on<C: ConnectionTrait + crate::config::ConfigConnection>(
    db: &C,
    actor: &str,
    row: &evidence::Model,
    retained_identity: bool,
    control: Option<&super::memory_control::PendingControl>,
    now: u64,
) -> Result<Option<DecodedEvidence>, AssistantStoreError> {
    let mode = if retained_identity {
        EvidenceReadMode::RetainedIdentity
    } else {
        EvidenceReadMode::Body
    };
    desk_diagnose_core::future::boxed(|| validate_on(db, actor, row, mode, now, control)).await
}

async fn validate_on<C: ConnectionTrait + crate::config::ConfigConnection>(
    db: &C,
    actor: &str,
    row: &evidence::Model,
    mode: EvidenceReadMode,
    now: u64,
    control: Option<&super::memory_control::PendingControl>,
) -> Result<Option<DecodedEvidence>, AssistantStoreError> {
    if row.actor_id != actor
        || (row.revoked_at_ms.is_some() && mode != EvidenceReadMode::OwnerHistoryIdentity)
        || row
            .expires_at_ms
            .is_some_and(|until| until <= 0 || until as u64 <= now)
        || row.retained_bytes <= 0
    {
        return Ok(None);
    }
    if row.body_cleared_at_ms.is_none() {
        if row.body.len() != row.retained_bytes as usize
            || row.content_sha256 != super::memory::content_hash_evidence(&row.body)
        {
            return Ok(None);
        }
    } else if mode == EvidenceReadMode::Body || !row.body.is_empty() {
        return Ok(None);
    }
    let Ok(policy) = serde_json::from_str::<SourceUsePolicy>(&row.policy_json) else {
        return Ok(None);
    };
    let Some(primary) = origin::Entity::find()
        .filter(origin::Column::ActorId.eq(actor))
        .filter(origin::Column::OriginId.eq(&row.origin_id))
        .one(db)
        .await?
    else {
        return Ok(None);
    };
    if !super::source_lifecycle::origin_readable_on(db, actor, &primary).await?
        || primary.statement_timezone != row.statement_timezone
        || primary.created_at_ms != row.observed_at_ms
        || (!primary.device_ref.is_empty()
            && super::memory::source_device_on(db, actor, &primary.device_ref)
                .await?
                .is_none())
    {
        return Ok(None);
    }
    let Ok(primary_policy) = serde_json::from_str::<SourceUsePolicy>(&primary.policy_json) else {
        return Ok(None);
    };
    let Some(memory_revision) = revision::Entity::find()
        .filter(revision::Column::ActorId.eq(actor))
        .filter(revision::Column::MemoryId.eq(&row.memory_id))
        .filter(revision::Column::Revision.eq(row.memory_revision))
        .one(db)
        .await?
    else {
        return Ok(None);
    };
    let memory_revision = match control {
        Some(control) => control.version_before(memory_revision)?,
        None => memory_revision,
    };
    if !(matches!(
        memory_revision.status.as_str(),
        "active" | "pending_conflict" | "superseded"
    ) || (mode == EvidenceReadMode::OwnerHistoryIdentity
        && memory_revision.status == "forgotten"))
        || memory_revision.expires_at_ms.is_some_and(|until| {
            until <= 0 || (mode == EvidenceReadMode::Body && until as u64 <= now)
        })
    {
        return Ok(None);
    }
    if let Some(cleared) = row.body_cleared_at_ms
        && (cleared <= 0
            || cleared as u64 > now
            || (!(mode == EvidenceReadMode::OwnerHistoryIdentity
                && memory_revision.status == "forgotten")
                && memory_revision
                    .expires_at_ms
                    .is_none_or(|until| until > cleared)))
    {
        return Ok(None);
    }
    let Ok(dependencies) = serde_json::from_str::<Vec<HistoryDependency>>(&row.dependencies_json)
    else {
        return Ok(None);
    };
    let Ok(known) = serde_json::from_str::<Vec<KnownMemorySource>>(&row.known_sources_json) else {
        return Ok(None);
    };
    let Ok(forget) = serde_json::from_str::<Vec<KnownMemorySource>>(&row.forget_sources_json)
    else {
        return Ok(None);
    };
    let Ok(proof) = serde_json::from_str::<MemoryEvidenceRetentionProof>(&row.retention_proof_json)
    else {
        return Ok(None);
    };
    let role = match row.role.as_str() {
        "evidence" => SourceRefRole::Evidence,
        "background" => SourceRefRole::Background,
        "previous_memory" => SourceRefRole::PreviousMemory,
        _ => return Ok(None),
    };
    let kind = match primary.kind.as_str() {
        "input" => AssistantSourceKind::Input,
        "assistant_message" => AssistantSourceKind::AssistantMessage,
        "receipt" => AssistantSourceKind::Receipt,
        "native_status" => AssistantSourceKind::NativeStatus,
        "previous_memory" => AssistantSourceKind::PreviousMemory,
        _ => return Ok(None),
    };
    let snapshot = MemoryEvidenceSnapshot {
        retained_origins: &proof.retained_origins,
        actor_id: actor,
        memory_id: &row.memory_id,
        memory_revision: unsigned(row.memory_revision)?,
        memory_accepted_input_seq: unsigned(memory_revision.accepted_input_seq)?,
        evidence_source_id: &row.source_id,
        role,
        body: &row.body,
        origin_id: &primary.origin_id,
        origin_kind: kind,
        origin_deletion_generation: unsigned(primary.deletion_generation)?,
        origin_accepted_input_seq: unsigned(primary.accepted_input_seq)?,
        origin_content_sha256: &primary.original_sha256,
        recorded_at_unix_ms: unsigned(row.observed_at_ms)?,
        statement_timezone: &row.statement_timezone,
        origin_policy: &primary_policy,
        evidence_policy: &policy,
        known_sources: &known,
        forget_sources: &forget,
        production_dependencies: &dependencies,
    };
    let expiry = row.expires_at_ms.map(unsigned).transpose()?;
    let valid = match mode {
        EvidenceReadMode::Body => proof.validate_snapshot(&snapshot, expiry, now),
        EvidenceReadMode::RetainedIdentity | EvidenceReadMode::OwnerHistoryIdentity => proof
            .validate_identity(
                &snapshot,
                &row.content_sha256,
                row.retained_bytes as u32,
                expiry,
                now,
            ),
    };
    if valid.is_err()
        || !retained_origins_allowed_on(
            db,
            actor,
            &proof.retained_origins,
            unsigned(memory_revision.accepted_input_seq)?,
            now,
        )
        .await?
    {
        return Ok(None);
    }
    // Known origins include original evidence carried through updates. Their
    // manual removal cannot be bypassed by retaining only a newer primary body.
    for source in &forget {
        if mode != EvidenceReadMode::OwnerHistoryIdentity
            && super::suppression::suppressed_known_on(db, actor, source).await?
            && control.is_none_or(|control| !control.owns_suppression(source))
        {
            return Ok(None);
        }
    }
    let mut checked_origins = std::collections::BTreeSet::new();
    for source in &known {
        if !checked_origins.insert(source.origin_id.as_str()) {
            continue;
        }
        let Some(known_origin) = origin::Entity::find()
            .filter(origin::Column::ActorId.eq(actor))
            .filter(origin::Column::OriginId.eq(&source.origin_id))
            .one(db)
            .await?
        else {
            return Ok(None);
        };
        if !super::source_lifecycle::origin_readable_on(db, actor, &known_origin).await? {
            return Ok(None);
        }
    }
    for dependency in &dependencies {
        if dependency
            .expires_at_unix_ms
            .is_some_and(|until| until <= now)
        {
            return Ok(None);
        }
        let Some(original) = origin::Entity::find()
            .filter(origin::Column::ActorId.eq(actor))
            .filter(origin::Column::OriginId.eq(&dependency.origin_id))
            .one(db)
            .await?
        else {
            return Ok(None);
        };
        if !super::source_lifecycle::origin_readable_on(db, actor, &original).await?
            || original.accepted_input_seq <= 0
            || original.accepted_input_seq > memory_revision.accepted_input_seq
            || unsigned(original.deletion_generation)? != dependency.origin_deletion_generation
            || (!original.device_ref.is_empty()
                && super::memory::source_device_on(db, actor, &original.device_ref)
                    .await?
                    .is_none())
        {
            return Ok(None);
        }
        let Ok(policy) = serde_json::from_str::<SourceUsePolicy>(&original.policy_json) else {
            return Ok(None);
        };
        let dependency_policy_revision = if let Some(source) = evidence::Entity::find()
            .filter(evidence::Column::ActorId.eq(actor))
            .filter(evidence::Column::SourceId.eq(&dependency.source_id))
            .one(db)
            .await?
        {
            if source.origin_id != dependency.origin_id
                || unsigned(source.memory_revision)? != dependency.source_revision
                || source.content_sha256 != dependency.content_sha256
                || (source.revoked_at_ms.is_some()
                    && mode != EvidenceReadMode::OwnerHistoryIdentity)
                || source.expires_at_ms.is_some_and(|until| {
                    until <= 0
                        || until as u64 <= now
                        || dependency
                            .expires_at_unix_ms
                            .is_none_or(|expiry| expiry > until as u64)
                })
            {
                return Ok(None);
            }
            let Ok(source_policy) = serde_json::from_str::<SourceUsePolicy>(&source.policy_json)
            else {
                return Ok(None);
            };
            if validate_policy(&source_policy).is_err() || !source_policy.independent_fact_allowed || source_policy.no_copy
                || matches!(source_policy.class, desk_agent_protocol::ai_assistant::source::SourcePolicyClass::Media
                    | desk_agent_protocol::ai_assistant::source::SourcePolicyClass::ProviderPrivate | desk_agent_protocol::ai_assistant::source::SourcePolicyClass::Unknown)
                || source_policy.content_hard_expires_at_unix_ms.is_some_and(|until| until <= now
                    || dependency.expires_at_unix_ms.is_none_or(|expiry| expiry > until)) { return Ok(None); }
            source_policy.policy_revision
        } else {
            policy.policy_revision
        };
        if dependency_policy_revision != dependency.policy_revision
            || validate_policy(&policy).is_err()
            || !policy.independent_fact_allowed
            || policy.no_copy
            || matches!(
                policy.class,
                desk_agent_protocol::ai_assistant::source::SourcePolicyClass::Media
                    | desk_agent_protocol::ai_assistant::source::SourcePolicyClass::ProviderPrivate
                    | desk_agent_protocol::ai_assistant::source::SourcePolicyClass::Unknown
            )
            || policy.content_hard_expires_at_unix_ms.is_some_and(|until| {
                until <= now
                    || dependency
                        .expires_at_unix_ms
                        .is_none_or(|expiry| expiry > until)
            })
        {
            return Ok(None);
        }
    }
    Ok(Some(DecodedEvidence {
        policy,
        primary,
        primary_policy,
        known,
        forget,
        dependencies,
    }))
}

/// Independent production metadata outlives natural chat cleanup. All hard
/// constraints still resolve through the original actor-scoped origin row.
pub(crate) fn retained_origins_allowed_on<
    'a,
    C: ConnectionTrait + crate::config::ConfigConnection,
>(
    db: &'a C,
    actor: &'a str,
    retained: &'a [RetainedMemoryOrigin],
    input_seq: u64,
    now: u64,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<bool, AssistantStoreError>> + Send + 'a>,
> {
    desk_diagnose_core::future::send_phase(move || async move {
        if canonical_retained_origins(retained).is_err() {
            return Ok(false);
        }
        for saved in retained {
            let Some(original) = origin::Entity::find()
                .filter(origin::Column::ActorId.eq(actor))
                .filter(origin::Column::OriginId.eq(&saved.origin_id))
                .one(db)
                .await?
            else {
                return Ok(false);
            };
            if !super::source_lifecycle::origin_readable_on(db, actor, &original).await?
                || unsigned(original.deletion_generation)? != saved.deletion_generation
                || unsigned(original.accepted_input_seq)? != saved.accepted_input_seq
                || saved.accepted_input_seq > input_seq
                || original.original_sha256 != saved.content_sha256
                || (!original.device_ref.is_empty()
                    && super::memory::source_device_on(db, actor, &original.device_ref)
                        .await?
                        .is_none())
            {
                return Ok(false);
            }
            let Ok(policy) = serde_json::from_str::<SourceUsePolicy>(&original.policy_json) else {
                return Ok(false);
            };
            if validate_policy(&policy).is_err() || policy != saved.policy
            || !policy.independent_fact_allowed || policy.no_copy
            || !matches!(policy.class, desk_agent_protocol::ai_assistant::source::SourcePolicyClass::UserText
                | desk_agent_protocol::ai_assistant::source::SourcePolicyClass::StructuredObservation)
            || saved.policy.content_hard_expires_at_unix_ms.is_some_and(|until| until <= now) { return Ok(false); }
        }
        Ok(true)
    })
}
