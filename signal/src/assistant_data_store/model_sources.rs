//! Live model-purpose source validation under the caller's native actor fence.
//! Owner-view DTOs, stored read proofs and old send grants grant no current use.

use super::{AssistantStoreError, signed, unsigned};
use crate::entity::{
    agent_assistant_history as history, agent_assistant_origin as origin,
    agent_assistant_search_document as document, agent_memory as memory,
    agent_memory_revision as revision, agent_memory_source as evidence,
};
use desk_agent_protocol::{
    ai_assistant::{
        history::{AssistantDeviceScope, HistoryAccessScope},
        memory::MemoryUsePurpose,
        source::{AssistantSourceKind, HistoryDependency, SourceReadPurpose, SourceUsePolicy},
        user_instructions::AssistantDataPolicy,
    },
    data_lineage::DestinationIdentity,
};
use desk_diagnose_core::ai_assistant::{
    data_access::AssistantDataToolAccess,
    data_control::CurrentMemoryUse,
    memory_control::KnownMemorySource,
    seam::AssistantDataReadUse,
    source::{HistoryRetentionProof, allows_independent_fact_use, allows_model_use},
};
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, QuerySelect};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

mod retained;
pub(crate) use retained::owner_memory_identity_sources_on;

#[derive(Clone, Copy, PartialEq, Eq)]
enum SourceContentUse {
    ReadBody,
    RetainedIdentity,
}

type SourceKey = (String, u64, String, String);
fn key(source: &HistoryDependency) -> SourceKey {
    (
        source.source_id.clone(),
        source.source_revision,
        source.origin_id.clone(),
        source.content_sha256.clone(),
    )
}

/// The deadline narrows this physical call. Its expiration does not erase
/// independent facts, native receipts or already accepted owner-visible history.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn validate_on<C: ConnectionTrait + crate::config::ConfigConnection>(
    db: &C,
    actor: &str,
    current_device: &str,
    usage: &AssistantDataReadUse,
    access: &AssistantDataToolAccess,
    scope: Option<&HistoryAccessScope>,
    destination: &DestinationIdentity,
    platform: &AssistantDataPolicy,
    now: u64,
) -> Result<Option<u64>, AssistantStoreError> {
    desk_diagnose_core::future::boxed(|| {
        validate_usage_on(
            db,
            actor,
            current_device,
            usage,
            access,
            scope,
            destination,
            platform,
            now,
            SourceContentUse::ReadBody,
            None,
            None,
        )
    })
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn validate_memory_devices_on<
    C: ConnectionTrait + crate::config::ConfigConnection,
>(
    db: &C,
    actor: &str,
    devices: &BTreeSet<String>,
    usage: &AssistantDataReadUse,
    access: &AssistantDataToolAccess,
    destination: &DestinationIdentity,
    platform: &AssistantDataPolicy,
    now: u64,
) -> Result<Option<u64>, AssistantStoreError> {
    if !matches!(
        usage.purpose,
        SourceReadPurpose::ModelMemory | SourceReadPurpose::MemoryEvidence
    ) {
        return Err(AssistantStoreError::Inaccessible);
    }
    desk_diagnose_core::future::boxed(|| {
        validate_usage_on(
            db,
            actor,
            "",
            usage,
            access,
            None,
            destination,
            platform,
            now,
            SourceContentUse::ReadBody,
            None,
            Some(devices),
        )
    })
    .await
}

#[allow(clippy::too_many_arguments)]
async fn validate_usage_on<C: ConnectionTrait + crate::config::ConfigConnection>(
    db: &C,
    actor: &str,
    current_device: &str,
    usage: &AssistantDataReadUse,
    access: &AssistantDataToolAccess,
    scope: Option<&HistoryAccessScope>,
    destination: &DestinationIdentity,
    platform: &AssistantDataPolicy,
    now: u64,
    content_use: SourceContentUse,
    control: Option<&super::memory_control::PendingControl>,
    memory_devices: Option<&BTreeSet<String>>,
) -> Result<Option<u64>, AssistantStoreError> {
    usage.validate().map_err(AssistantStoreError::Invalid)?;
    access.validate().map_err(AssistantStoreError::Invalid)?;
    platform.validate().map_err(AssistantStoreError::Invalid)?;
    if usage.policy_revision != access.policy_revision
        || usage.policy_revision != platform.revision
        || usage.control_generation != access.control_generation
        || usage.memory_state_generation != access.memory_state_generation
        || usage.expires_at_unix_ms.is_some_and(|until| until <= now)
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    let independent = match usage.purpose {
        SourceReadPurpose::OwnerHistory => return Err(AssistantStoreError::Inaccessible),
        SourceReadPurpose::ModelHistory => {
            if !access.settings.model_history_search_enabled || !usage.memories.is_empty() {
                return Err(AssistantStoreError::Inaccessible);
            }
            let scope = scope.ok_or(AssistantStoreError::Inaccessible)?;
            scope.validate().map_err(AssistantStoreError::Invalid)?;
            if scope.actor_id != actor
                || scope.current_device_ref != current_device
                || scope.retrieval_episode_id != usage.retrieval_episode_id
                || scope.expires_at_unix_ms.is_some_and(|until| until <= now)
            {
                return Err(AssistantStoreError::Inaccessible);
            }
            false
        }
        SourceReadPurpose::MemoryEvidence | SourceReadPurpose::ModelMemory => {
            if !access.settings.automatic_memory_enabled {
                return Err(AssistantStoreError::Inaccessible);
            }
            true
        }
    };
    let mut deadline = usage.expires_at_unix_ms;
    let mut covered = BTreeMap::new();
    let mut memory_roots = BTreeSet::new();
    let mut fact_producer_inputs = Vec::new();
    for reference in &usage.memories {
        let (rows, producer_dependencies, until) = match content_use {
            SourceContentUse::ReadBody => {
                desk_diagnose_core::future::boxed(|| {
                    memory_sources_with_control_on(
                        db,
                        actor,
                        current_device,
                        reference,
                        now,
                        control,
                        memory_devices,
                    )
                })
                .await?
            }
            SourceContentUse::RetainedIdentity => {
                retained::memory_identity_sources_on(db, actor, reference).await?
            }
        };
        deadline = earlier(deadline, until);
        if usage.purpose != SourceReadPurpose::MemoryEvidence {
            for source in producer_dependencies {
                fact_producer_inputs.push(source.clone());
                if let Some(old) = covered.insert(key(&source), source.clone())
                    && old != source
                {
                    return Err(AssistantStoreError::Invalid(
                        "memory producer dependency has conflicting constraints",
                    ));
                }
            }
        }
        for row in rows {
            if usage.purpose == SourceReadPurpose::MemoryEvidence
                && !usage.roots.iter().chain(&usage.decision_roots).any(|root| {
                    root.source_id == row.source_id
                        && root.source_revision == row.memory_revision as u64
                })
            {
                continue;
            }
            let (own, dependencies, until) = desk_diagnose_core::future::boxed(|| {
                evidence_with_control_on(
                    db,
                    actor,
                    &row,
                    content_use == SourceContentUse::RetainedIdentity,
                    destination,
                    now,
                    control,
                )
            })
            .await?;
            memory_roots.insert((own.source_id.clone(), own.source_revision));
            deadline = earlier(deadline, until);
            for source in std::iter::once(own).chain(dependencies) {
                if let Some(old) = covered.insert(key(&source), source.clone())
                    && old != source
                {
                    return Err(AssistantStoreError::Invalid(
                        "memory source dependency has conflicting constraints",
                    ));
                }
            }
        }
    }
    if independent
        && (memory_roots.len() != usage.roots.len()
            || usage.roots.iter().chain(&usage.decision_roots).any(|root| {
                !memory_roots.contains(&(root.source_id.clone(), root.source_revision))
            }))
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    if independent
        && usage
            .history_dependencies
            .iter()
            .any(|dependency| covered.get(&key(dependency)) != Some(dependency))
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    // Validate every retained memory source even if a malformed projection omitted
    // it from its declared dependency set. A valid proof must declare the exact
    // complete closure, so neither undeclared constraints nor extra sources pass.
    if independent
        && (covered.len() != usage.history_dependencies.len()
            || covered.keys().any(|source| {
                !usage
                    .history_dependencies
                    .iter()
                    .any(|dependency| &key(dependency) == source)
            }))
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    let declared = usage
        .history_dependencies
        .iter()
        .map(|source| (key(source), source))
        .collect::<BTreeMap<_, _>>();
    let mut queue = VecDeque::new();
    for root in usage.roots.iter().chain(&usage.decision_roots) {
        let source = usage
            .history_dependencies
            .iter()
            .find(|source| {
                source.source_id == root.source_id && source.source_revision == root.source_revision
            })
            .ok_or(AssistantStoreError::Inaccessible)?;
        queue.push_back((source.clone(), independent));
    }
    queue.extend(
        fact_producer_inputs
            .into_iter()
            .map(|source| (source, true)),
    );
    let mut seen = BTreeMap::<SourceKey, (HistoryDependency, bool)>::new();
    while let Some((dependency, mut fact_evidence)) = queue.pop_front() {
        let source_key = key(&dependency);
        if declared.get(&source_key).copied() != Some(&dependency) {
            return Err(AssistantStoreError::Invalid(
                "read projection omitted an actual producer dependency",
            ));
        }
        if let Some((previous, was_independent)) = seen.get(&source_key) {
            if previous != &dependency {
                return Err(AssistantStoreError::Invalid(
                    "source dependency constraints changed",
                ));
            }
            if *was_independent == fact_evidence || !*was_independent {
                continue;
            }
        }
        // Independent evidence checks its exact forgetting targets itself.
        // Production inputs still enforce deletion, expiry and source policy.
        if !fact_evidence
            && super::suppression::suppressed_on(db, actor, &dependency).await?
            && !control_owns(control, &dependency)
        {
            return Err(AssistantStoreError::Inaccessible);
        }
        let retained_fact = evidence::Entity::find()
            .filter(evidence::Column::ActorId.eq(actor))
            .filter(evidence::Column::SourceId.eq(&dependency.source_id))
            .one(db)
            .await?;
        let evidence_policy = retained_fact.is_some();
        if let Some(row) = retained_fact {
            if usage.purpose == SourceReadPurpose::ModelHistory
                && usage.roots.iter().chain(&usage.decision_roots).any(|root| {
                    root.source_id == dependency.source_id
                        && root.source_revision == dependency.source_revision
                })
            {
                return Err(AssistantStoreError::Inaccessible);
            }
            if unsigned(row.memory_revision)? != dependency.source_revision
                || row.origin_id != dependency.origin_id
                || row.content_sha256 != dependency.content_sha256
            {
                return Err(AssistantStoreError::Inaccessible);
            }
            let direct = usage.roots.iter().chain(&usage.decision_roots).any(|root| {
                root.source_id == dependency.source_id
                    && root.source_revision == dependency.source_revision
            });
            let (own, dependencies, until) = desk_diagnose_core::future::boxed(|| {
                evidence_with_control_on(
                    db,
                    actor,
                    &row,
                    !direct || content_use == SourceContentUse::RetainedIdentity,
                    destination,
                    now,
                    control,
                )
            })
            .await?;
            if own != dependency {
                return Err(AssistantStoreError::Inaccessible);
            }
            deadline = earlier(deadline, until);
            fact_evidence = true;
            for source in dependencies {
                queue.push_back((source, true));
            }
        } else {
            let row = history::Entity::find()
                .filter(history::Column::ActorId.eq(actor))
                .filter(history::Column::SourceId.eq(&dependency.source_id))
                .one(db)
                .await?;
            if let Some(row) = row {
                if unsigned(row.revision)? != dependency.source_revision
                    || row.content_sha256 != dependency.content_sha256
                    || row.origin_id != dependency.origin_id
                    || row.deleted_at_ms.is_some()
                {
                    return Err(AssistantStoreError::Inaccessible);
                }
                super::history_native::read(&row)?;
                if !fact_evidence {
                    let history_scope = scope.ok_or(AssistantStoreError::Inaccessible)?;
                    deadline = earlier(
                        deadline,
                        Some(
                            desk_diagnose_core::future::send_phase(|| {
                                validate_history_on(
                                    db,
                                    actor,
                                    &row,
                                    history_scope,
                                    destination,
                                    platform,
                                    now,
                                )
                            })
                            .await?,
                        ),
                    );
                    let sources: Vec<HistoryDependency> =
                        decode_dependencies(&row.dependencies_json)?;
                    for source in sources {
                        queue.push_back((source, false));
                    }
                }
            } else if !fact_evidence {
                return Err(AssistantStoreError::Inaccessible);
            }
        }
        deadline = earlier(
            deadline,
            origin_on(
                db,
                actor,
                &dependency,
                fact_evidence,
                evidence_policy,
                destination,
                now,
            )
            .await?,
        );
        seen.insert(source_key, (dependency, fact_evidence));
    }
    if seen.len() != declared.len() {
        return Err(AssistantStoreError::Invalid(
            "read projection contains unreachable producer inputs",
        ));
    }
    if deadline.is_some_and(|until| until <= now) {
        return Err(AssistantStoreError::Inaccessible);
    }
    Ok(deadline)
}

pub(super) async fn memory_sources_on<C: ConnectionTrait + crate::config::ConfigConnection>(
    db: &C,
    actor: &str,
    current_device: &str,
    reference: &CurrentMemoryUse,
    now: u64,
) -> Result<(Vec<evidence::Model>, Vec<HistoryDependency>, Option<u64>), AssistantStoreError> {
    memory_sources_with_control_on(db, actor, current_device, reference, now, None, None).await
}

async fn memory_sources_with_control_on<C: ConnectionTrait + crate::config::ConfigConnection>(
    db: &C,
    actor: &str,
    current_device: &str,
    reference: &CurrentMemoryUse,
    now: u64,
    control: Option<&super::memory_control::PendingControl>,
    memory_devices: Option<&BTreeSet<String>>,
) -> Result<(Vec<evidence::Model>, Vec<HistoryDependency>, Option<u64>), AssistantStoreError> {
    let fact = memory::Entity::find()
        .filter(memory::Column::ActorId.eq(actor))
        .filter(memory::Column::MemoryId.eq(&reference.memory_id))
        .one(db)
        .await?
        .ok_or(AssistantStoreError::Inaccessible)?;
    let fact = match control {
        Some(control) => control.head_before(fact)?,
        None => fact,
    };
    if fact.status == "forgotten"
        || fact.revision <= 0
        || reference.revision > unsigned(fact.revision)?
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    if fact.scope_key == "personal" {
        if !fact.device_ref.is_empty() {
            return Err(AssistantStoreError::Inaccessible);
        }
    } else if fact.scope_key != format!("device:{}", fact.device_ref)
        || super::memory::source_device_on(db, actor, &fact.device_ref)
            .await?
            .is_none()
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    if !fact.device_ref.is_empty()
        && memory_devices.is_some_and(|devices| !devices.contains(&fact.device_ref))
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    if reference.purpose == MemoryUsePurpose::CurrentFact
        && (fact.status != "active"
            || reference.revision != unsigned(fact.revision)?
            || fact.valid_from_ms < 0
            || fact.valid_from_ms as u64 > now
            || fact
                .valid_until_ms
                .is_some_and(|until| until <= 0 || until as u64 <= now)
            || (memory_devices.is_none()
                && !fact.device_ref.is_empty()
                && fact.device_ref != current_device))
    {
        return Err(AssistantStoreError::Inaccessible);
    }

    let original = revision::Entity::find()
        .filter(revision::Column::ActorId.eq(actor))
        .filter(revision::Column::MemoryId.eq(&reference.memory_id))
        .filter(revision::Column::Revision.eq(signed(reference.revision)?))
        .one(db)
        .await?
        .ok_or(AssistantStoreError::Inaccessible)?;
    let original = match control {
        Some(control) => control.version_before(original)?,
        None => original,
    };
    if !matches!(
        original.status.as_str(),
        "active" | "pending_conflict" | "superseded"
    ) || original.body.trim().is_empty()
        || original.content_sha256
            != super::memory::content_hash(&original.body, original.context.as_deref())
        || original.accepted_input_seq <= 0
        || original.created_at_ms <= 0
        || original
            .expires_at_ms
            .is_some_and(|until| until <= 0 || until as u64 <= now)
        || (reference.revision < unsigned(fact.revision)? && original.expires_at_ms.is_none())
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    if reference.revision == unsigned(fact.revision)?
        && (original.body != fact.body
            || original.context != fact.context
            || original.content_sha256 != fact.content_sha256
            || original.status != fact.status
            || original.accepted_input_seq != fact.accepted_input_seq
            || original.state_generation != fact.state_generation)
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    let rows = evidence::Entity::find()
        .filter(evidence::Column::ActorId.eq(actor))
        .filter(evidence::Column::MemoryId.eq(&reference.memory_id))
        .filter(evidence::Column::MemoryRevision.eq(signed(reference.revision)?))
        .all(db)
        .await?;
    if rows.is_empty() || !rows.iter().any(|row| row.role == "evidence") {
        return Err(AssistantStoreError::Inaccessible);
    }
    let expiry = original.expires_at_ms.map(unsigned).transpose()?;
    let dependencies = decode_dependencies(&original.dependencies_json)?;
    Ok((
        rows,
        dependencies,
        if reference.purpose == MemoryUsePurpose::CurrentFact {
            earlier(expiry, fact.valid_until_ms.map(unsigned).transpose()?)
        } else {
            expiry
        },
    ))
}

pub(super) async fn evidence_on<C: ConnectionTrait + crate::config::ConfigConnection>(
    db: &C,
    actor: &str,
    row: &evidence::Model,
    retained_copy: bool,
    destination: &DestinationIdentity,
    now: u64,
) -> Result<(HistoryDependency, Vec<HistoryDependency>, Option<u64>), AssistantStoreError> {
    evidence_with_control_on(db, actor, row, retained_copy, destination, now, None).await
}

async fn evidence_with_control_on<C: ConnectionTrait + crate::config::ConfigConnection>(
    db: &C,
    actor: &str,
    row: &evidence::Model,
    retained_copy: bool,
    destination: &DestinationIdentity,
    now: u64,
    control: Option<&super::memory_control::PendingControl>,
) -> Result<(HistoryDependency, Vec<HistoryDependency>, Option<u64>), AssistantStoreError> {
    let before = control
        .map(|control| control.source_before(row.clone()))
        .transpose()?;
    let row = before.as_ref().unwrap_or(row);
    let decoded = super::evidence::load_on(db, actor, row, retained_copy, control, now)
        .await?
        .ok_or(AssistantStoreError::Inaccessible)?;
    let fact = memory::Entity::find()
        .filter(memory::Column::ActorId.eq(actor))
        .filter(memory::Column::MemoryId.eq(&row.memory_id))
        .one(db)
        .await?
        .ok_or(AssistantStoreError::Inaccessible)?;
    let fact = match control {
        Some(control) => control.head_before(fact)?,
        None => fact,
    };
    if fact.status == "forgotten" || row.memory_revision > fact.revision {
        return Err(AssistantStoreError::Inaccessible);
    }
    let policy = decoded.policy;
    // Retention proofs validate original assertions; this policy describes derived saved text.
    if !allows_model_use(&policy, destination, now) {
        return Err(AssistantStoreError::Inaccessible);
    }
    let original = origin::Entity::find()
        .filter(origin::Column::ActorId.eq(actor))
        .filter(origin::Column::OriginId.eq(&row.origin_id))
        .one(db)
        .await?
        .ok_or(AssistantStoreError::Inaccessible)?;
    let original_policy = if original == decoded.primary {
        decoded.primary_policy
    } else {
        serde_json::from_str(&original.policy_json)
            .map_err(|_| AssistantStoreError::Inaccessible)?
    };
    if !allows_model_use(&original_policy, destination, now) {
        return Err(AssistantStoreError::Inaccessible);
    }
    for source in decoded.forget {
        if super::suppression::suppressed_known_on(db, actor, &source).await?
            && control.is_none_or(|control| !control.owns_suppression(&source))
        {
            return Err(AssistantStoreError::Inaccessible);
        }
    }
    let dependencies = decoded.dependencies;
    let expiry = earlier(
        row.expires_at_ms.map(unsigned).transpose()?,
        earlier(
            policy.content_hard_expires_at_unix_ms,
            original_policy.content_hard_expires_at_unix_ms,
        ),
    );
    let own = HistoryDependency {
        source_id: row.source_id.clone(),
        source_revision: unsigned(row.memory_revision)?,
        content_sha256: row.content_sha256.clone(),
        origin_id: row.origin_id.clone(),
        origin_deletion_generation: unsigned(original.deletion_generation)?,
        policy_revision: policy.policy_revision,
        expires_at_unix_ms: expiry,
    };
    if super::suppression::suppressed_on(db, actor, &own).await? && !control_owns(control, &own) {
        return Err(AssistantStoreError::Inaccessible);
    }
    if retained_copy {
        return Ok((own, dependencies, expiry));
    }
    let retained = revision::Entity::find()
        .filter(revision::Column::ActorId.eq(actor))
        .filter(revision::Column::MemoryId.eq(&row.memory_id))
        .filter(revision::Column::Revision.eq(row.memory_revision))
        .one(db)
        .await?
        .ok_or(AssistantStoreError::Inaccessible)?;
    let retained = match control {
        Some(control) => control.version_before(retained)?,
        None => retained,
    };
    Ok((
        own,
        dependencies,
        earlier(expiry, retained.expires_at_ms.map(unsigned).transpose()?),
    ))
}

async fn origin_on<C: ConnectionTrait + crate::config::ConfigConnection>(
    db: &C,
    actor: &str,
    source: &HistoryDependency,
    independent: bool,
    evidence_policy: bool,
    destination: &DestinationIdentity,
    now: u64,
) -> Result<Option<u64>, AssistantStoreError> {
    let row = origin::Entity::find()
        .filter(origin::Column::ActorId.eq(actor))
        .filter(origin::Column::OriginId.eq(&source.origin_id))
        .one(db)
        .await?
        .ok_or(AssistantStoreError::Inaccessible)?;
    if !super::source_lifecycle::origin_readable_on(db, actor, &row).await?
        || unsigned(row.deletion_generation)? != source.origin_deletion_generation
        || source.expires_at_unix_ms.is_some_and(|until| until <= now)
        || (!row.device_ref.is_empty()
            && super::memory::source_device_on(db, actor, &row.device_ref)
                .await?
                .is_none())
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    let policy: SourceUsePolicy =
        serde_json::from_str(&row.policy_json).map_err(|_| AssistantStoreError::Inaccessible)?;
    if (!evidence_policy && policy.policy_revision != source.policy_revision)
        || !(if independent && !evidence_policy {
            allows_independent_fact_use(&policy, destination, now)
        } else {
            allows_model_use(&policy, destination, now)
        })
        || policy.content_hard_expires_at_unix_ms.is_some_and(|until| {
            source
                .expires_at_unix_ms
                .is_none_or(|expiry| expiry > until)
        })
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    Ok(earlier(
        source.expires_at_unix_ms,
        policy.content_hard_expires_at_unix_ms,
    ))
}

async fn validate_history_on<C: ConnectionTrait + crate::config::ConfigConnection>(
    db: &C,
    actor: &str,
    row: &history::Model,
    scope: &HistoryAccessScope,
    destination: &DestinationIdentity,
    platform: &AssistantDataPolicy,
    now: u64,
) -> Result<u64, AssistantStoreError> {
    let kind = match row.kind.as_str() {
        "input" => AssistantSourceKind::Input,
        "assistant_message" => AssistantSourceKind::AssistantMessage,
        "receipt" => AssistantSourceKind::Receipt,
        "native_status" => AssistantSourceKind::NativeStatus,
        _ => return Err(AssistantStoreError::Inaccessible),
    };
    let natural = unsigned(row.recorded_at_ms)?
        .checked_add(u64::from(platform.history_retention_days) * 86_400_000)
        .ok_or(AssistantStoreError::Inaccessible)?;
    if !scope.allowed_kinds.contains(&kind)
        || (scope.device_rule == AssistantDeviceScope::CurrentDevice
            && row.device_ref != scope.current_device_ref)
        || scope.time_range.as_ref().is_some_and(|range| {
            row.recorded_at_ms < 0
                || (row.recorded_at_ms as u64) < range.from_unix_ms
                || row.recorded_at_ms as u64 >= range.until_unix_ms
        })
        || row.deleted_at_ms.is_some()
        || natural <= now
        || row.expires_at_ms <= 0
        || row.expires_at_ms as u64 <= now
        || row.content.len() > 4096
        || row.content_sha256 != format!("{:x}", Sha256::digest(row.content.as_bytes()))
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    let original = origin::Entity::find()
        .filter(origin::Column::ActorId.eq(actor))
        .filter(origin::Column::OriginId.eq(&row.origin_id))
        .one(db)
        .await?
        .ok_or(AssistantStoreError::Inaccessible)?;
    let policy: SourceUsePolicy = serde_json::from_str(&original.policy_json)
        .map_err(|_| AssistantStoreError::Inaccessible)?;
    let proof: HistoryRetentionProof = serde_json::from_str(&row.retention_proof_json)
        .map_err(|_| AssistantStoreError::Inaccessible)?;
    let native = super::history_native::read(row)?;
    let native_without_owner_input =
        super::history_native::zero_input_allowed(row, native.as_ref());
    let dependencies = decode_dependencies(&row.dependencies_json)?;
    desk_diagnose_core::ai_assistant::source::validate_history_dependencies(&dependencies)
        .map_err(AssistantStoreError::Invalid)?;
    if !super::source_lifecycle::origin_readable_on(db, actor, &original).await?
        || !original.history_admitted
        || original.device_ref != row.device_ref
        || original.conversation_id != row.conversation_id
        || original.root_conversation_id != row.root_conversation_id
        || original.native_id != row.native_id
        || original.kind != row.kind
        || original.statement_timezone != row.statement_timezone
        || (!row.device_ref.is_empty()
            && super::memory::source_device_on(db, actor, &row.device_ref)
                .await?
                .is_none())
        || !allows_model_use(&policy, destination, now)
        || proof.actor_id != actor
        || proof.origin_id != row.origin_id
        || proof.device_ref != row.device_ref
        || (proof.accepted_input_seq == 0 && !native_without_owner_input)
        || proof.accepted_input_seq != unsigned(original.accepted_input_seq)?
        || proof.source_policy != policy
        || proof.recorded_at_unix_ms != unsigned(row.recorded_at_ms)?
        || proof.statement_timezone != row.statement_timezone
        || !desk_diagnose_core::ai_assistant::source::valid_statement_timezone(
            &row.statement_timezone,
        )
        || proof.expires_at_unix_ms != unsigned(row.expires_at_ms)?
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    let docs = document::Entity::find()
        .filter(document::Column::ActorId.eq(actor))
        .filter(document::Column::SourceKind.eq("history"))
        .filter(document::Column::SourceId.eq(&row.source_id))
        .filter(document::Column::SourceRevision.eq(row.revision))
        .filter(document::Column::ContentSha256.eq(&row.content_sha256))
        .limit(2)
        .all(db)
        .await?;
    let [doc] = docs.as_slice() else {
        return Err(AssistantStoreError::Inaccessible);
    };
    if doc.origin_id != row.origin_id
        || doc.device_ref != row.device_ref
        || doc.conversation_id != row.conversation_id
        || doc.root_conversation_id != row.root_conversation_id
        || doc.fragment_index != row.fragment_index
        || doc.byte_start < 0
        || doc.byte_end < doc.byte_start
        || doc.byte_end - doc.byte_start != row.content.len() as i32
        || doc.expires_at_ms != Some(row.expires_at_ms)
        || unsigned(doc.policy_revision)? != policy.policy_revision
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    Ok(natural.min(row.expires_at_ms as u64))
}

pub(super) fn decode_dependencies(
    value: &str,
) -> Result<Vec<HistoryDependency>, AssistantStoreError> {
    let values: Vec<HistoryDependency> =
        serde_json::from_str(value).map_err(|_| AssistantStoreError::Inaccessible)?;
    Ok(values)
}

fn earlier(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// Recheck the original session's source scope and live device access. The
/// native control fence can also supply its own pre-control fact versions.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn validate_session_sources_on<
    C: ConnectionTrait + crate::config::ConfigConnection,
>(
    db: &C,
    session: &desk_diagnose_core::session::PersistedAgentSession,
    usage: &AssistantDataReadUse,
    access: &AssistantDataToolAccess,
    destination: &DestinationIdentity,
    platform: &AssistantDataPolicy,
    now: u64,
    control: Option<&super::memory_control::PendingControl>,
) -> Result<Option<u64>, AssistantStoreError> {
    let data = session
        .assistant_data
        .as_ref()
        .ok_or(AssistantStoreError::Inaccessible)?;
    let devices = if session.has_admitted_personal_input() && !usage.memories.is_empty() {
        let scope =
            desk_diagnose_core::ai_assistant::memory_access::usable_memory_scope(session, now)
                .ok_or(AssistantStoreError::Inaccessible)?;
        Some(
            desk_diagnose_core::future::boxed(|| {
                super::current_memory::fact_devices_on(db, session, scope)
            })
            .await?,
        )
    } else {
        None
    };
    desk_diagnose_core::future::boxed(|| {
        validate_usage_on(
            db,
            &session.actor_id,
            &data.source_device_ref,
            usage,
            access,
            data.history_access_scope.as_deref(),
            destination,
            platform,
            now,
            SourceContentUse::ReadBody,
            control,
            devices.as_ref(),
        )
    })
    .await
}

fn control_owns(
    control: Option<&super::memory_control::PendingControl>,
    dependency: &HistoryDependency,
) -> bool {
    control.is_some_and(|control| {
        control.owns_suppression(&KnownMemorySource {
            source_id: dependency.source_id.clone(),
            revision: dependency.source_revision,
            origin_id: dependency.origin_id.clone(),
            content_sha256: dependency.content_sha256.clone(),
        })
    })
}
