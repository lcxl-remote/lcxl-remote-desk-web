//! Current copy restrictions for an already accepted, consumed native read.
//! This path returns no source body and grants no model or device operation.

use super::{AssistantStoreError, signed, unsigned};
use crate::entity::{
    agent_assistant_history as history, agent_assistant_origin as origin,
    agent_memory_source as evidence,
};
use desk_agent_protocol::ai_assistant::source::{
    HistoryDependency, SourcePolicyClass, SourceReadPurpose, SourceUsePolicy,
};
use desk_diagnose_core::ai_assistant::{
    retained_copy::OwnerHistoryReadValidation,
    source::{derive_visible_text_policy, validate_policy},
};
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter};
use std::collections::BTreeSet;

pub(crate) struct RetainedReadCopy {
    pub policy: SourceUsePolicy,
}

/// The caller holds its original actor/device/native fence. Every query here
/// precedes the final history quota mutation in that same transaction.
pub(crate) async fn inspect_on<C: ConnectionTrait + crate::config::ConfigConnection>(
    db: &C,
    view: &OwnerHistoryReadValidation,
    now: u64,
) -> Result<Option<RetainedReadCopy>, AssistantStoreError> {
    let usage = view.usage();
    if now == 0
        || now > i64::MAX as u64
        || usage.expires_at_unix_ms.is_some_and(|until| until <= now)
        || (!view.source_device_ref().is_empty()
            && super::memory::source_device_on(db, view.actor_id(), view.source_device_ref())
                .await?
                .is_none())
    {
        return Ok(None);
    }
    let mut evidence_roots = BTreeSet::new();
    for reference in &usage.memories {
        let result =
            super::model_sources::owner_memory_identity_sources_on(db, view, reference).await;
        let (rows, producer_inputs, _) = match result {
            Ok(value) => value,
            Err(AssistantStoreError::Inaccessible) => return Ok(None),
            Err(error) => return Err(error),
        };
        if usage.purpose != SourceReadPurpose::MemoryEvidence
            && producer_inputs
                .iter()
                .any(|source| !usage.history_dependencies.contains(source))
        {
            return Ok(None);
        }
        let mut selected = false;
        for row in rows {
            let root = usage.roots.iter().any(|source| {
                source.source_id == row.source_id
                    && source.source_revision == row.memory_revision as u64
            });
            if usage.purpose == SourceReadPurpose::MemoryEvidence && !root {
                continue;
            }
            if !root {
                return Ok(None);
            }
            selected = true;
            evidence_roots.insert((row.source_id.clone(), unsigned(row.memory_revision)?));
            let expiry = row.expires_at_ms.map(unsigned).transpose()?;
            if !usage.history_dependencies.iter().any(|source| {
                source.source_id == row.source_id
                    && source.source_revision == row.memory_revision as u64
                    && source.origin_id == row.origin_id
                    && source.expires_at_unix_ms == expiry
            }) {
                return Ok(None);
            }
        }
        if !selected {
            return Ok(None);
        }
    }
    match usage.purpose {
        SourceReadPurpose::ModelMemory | SourceReadPurpose::MemoryEvidence => {
            if evidence_roots.len() != usage.roots.len() || !usage.decision_roots.is_empty() {
                return Ok(None);
            }
        }
        SourceReadPurpose::ModelHistory => {
            if !usage.memories.is_empty() {
                return Ok(None);
            }
        }
        SourceReadPurpose::OwnerHistory => return Ok(None),
    }
    let mut policies = std::collections::BTreeSet::new();
    for source in &usage.history_dependencies {
        if source.expires_at_unix_ms.is_some_and(|until| until <= now) {
            return Ok(None);
        }
        let Some(original) = origin::Entity::find()
            .filter(origin::Column::ActorId.eq(view.actor_id()))
            .filter(origin::Column::OriginId.eq(&source.origin_id))
            .one(db)
            .await?
        else {
            return Ok(None);
        };
        if !super::source_lifecycle::origin_readable_on(db, view.actor_id(), &original).await?
            || original.deletion_generation != signed(source.origin_deletion_generation)?
            || original.accepted_input_seq <= 0
            || original.accepted_input_seq as u64 > view.actor_input_seq()
            || (!original.device_ref.is_empty()
                && super::memory::source_device_on(db, view.actor_id(), &original.device_ref)
                    .await?
                    .is_none())
        {
            return Ok(None);
        }
        let Some(original_policy) = copy_policy(&original.policy_json, now) else {
            return Ok(None);
        };
        let retained = evidence::Entity::find()
            .filter(evidence::Column::ActorId.eq(view.actor_id()))
            .filter(evidence::Column::SourceId.eq(&source.source_id))
            .one(db)
            .await?;
        let policy_revision = if let Some(row) = retained {
            if row.origin_id != source.origin_id
                || row.memory_revision != signed(source.source_revision)?
                || !super::evidence::owner_history_identity_allowed_on(
                    db,
                    view.actor_id(),
                    &row,
                    now,
                )
                .await?
            {
                return Ok(None);
            }
            if usage.purpose == SourceReadPurpose::ModelHistory
                && usage.roots.iter().chain(&usage.decision_roots).any(|root| {
                    root.source_id == source.source_id
                        && root.source_revision == source.source_revision
                })
            {
                return Ok(None);
            }
            let Some(policy) = copy_policy(&row.policy_json, now) else {
                return Ok(None);
            };
            if !dependencies_declared(&row.dependencies_json, &usage.history_dependencies) {
                return Ok(None);
            }
            let revision = policy.policy_revision;
            policies.insert(policy);
            revision
        } else {
            // A naturally reclaimed history body is not required again: its
            // exact consumed identity is sealed in the accepted native proof.
            // A live contradictory row or manual source deletion still denies.
            if let Some(row) = history::Entity::find()
                .filter(history::Column::ActorId.eq(view.actor_id()))
                .filter(history::Column::SourceId.eq(&source.source_id))
                .one(db)
                .await?
            {
                if row.origin_id != source.origin_id
                    || row.revision != signed(source.source_revision)?
                    || row.deleted_at_ms.is_some()
                    || !dependencies_declared(&row.dependencies_json, &usage.history_dependencies)
                {
                    return Ok(None);
                }
                if super::history_native::read(&row).is_err() {
                    return Ok(None);
                }
            }
            original_policy.policy_revision
        };
        if policy_revision != source.policy_revision {
            return Ok(None);
        }
        policies.insert(original_policy);
    }
    // A typed empty result has no source body. This baseline carries only the
    // original explicit deadline; all actual source policies narrow it below.
    let baseline = SourceUsePolicy {
        policy_revision: usage.policy_revision,
        class: SourcePolicyClass::AssistantText,
        history_retention_allowed: true,
        independent_fact_allowed: false,
        derived_visible_text_allowed: true,
        no_copy: false,
        no_export: false,
        content_hard_expires_at_unix_ms: None,
        reference_fresh_until_unix_ms: None,
        fixed_destinations: Vec::new(),
    };
    policies.insert(baseline);
    Ok(Some(RetainedReadCopy {
        policy: derive_visible_text_policy(
            &policies.into_iter().collect::<Vec<_>>(),
            usage.policy_revision,
        )
        .map_err(AssistantStoreError::Invalid)?,
    }))
}

fn copy_policy(raw: &str, now: u64) -> Option<SourceUsePolicy> {
    let policy: SourceUsePolicy = serde_json::from_str(raw).ok()?;
    (validate_policy(&policy).is_ok()
        && policy.derived_visible_text_allowed
        && !policy.no_copy
        && !matches!(
            policy.class,
            SourcePolicyClass::ProviderPrivate | SourcePolicyClass::Unknown
        )
        && policy
            .content_hard_expires_at_unix_ms
            .is_none_or(|until| until > now))
    .then_some(policy)
}

fn dependencies_declared(raw: &str, declared: &[HistoryDependency]) -> bool {
    let Ok(sources) = serde_json::from_str::<Vec<HistoryDependency>>(raw) else {
        return false;
    };
    sources.iter().all(|source| declared.contains(source))
}
