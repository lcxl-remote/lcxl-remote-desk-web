//! Synchronous storage of model-authored memories. The host owns admission and the transaction.

use crate::entity::{
    agent_assistant_origin as origin, agent_assistant_runtime as runtime, agent_memory as memory,
    agent_memory_commit as receipt, agent_memory_revision as revision,
    agent_memory_source as source,
};
use crate::{
    AssistantStoreError, increment, memory_index, memory_quota as quota, signed, unsigned,
};
use desk_agent_protocol::ai_assistant::{
    memory::*, source::*, user_instructions::AssistantDataPolicy,
};
use desk_diagnose_core::{
    ai_assistant::{
        memory_control::KnownMemorySource,
        memory_evidence::{MemoryEvidenceRetentionProof, MemoryEvidenceSnapshot},
    },
    native_ids::{self, Kind},
};
use sea_orm::{
    ActiveModelTrait, ActiveValue, ColumnTrait, ConnectionTrait, EntityTrait, PaginatorTrait,
    QueryFilter, sea_query::Expr,
};

pub mod receipts;
pub mod sources;

pub fn source_matches_device(
    origin: &origin::Model,
    target: &str,
    personal_root: Option<&str>,
) -> bool {
    target.is_empty()
        || origin.device_ref == target
        || (origin.device_ref.is_empty()
            && origin.kind == "input"
            && personal_root == Some(origin.conversation_id.as_str()))
}

/// A retained fact can be scoped to a device while its original owner message
/// came from a personal conversation. Keep that message's provenance intact.
pub async fn retained_source_matches_device_on<C: ConnectionTrait>(
    db: &C,
    row: &source::Model,
    origin: &origin::Model,
    target: &str,
    personal_root: Option<&str>,
) -> Result<bool, AssistantStoreError> {
    if personal_root.is_none() || target.is_empty() {
        return Ok(source_matches_device(origin, target, personal_root));
    }
    let fact = memory::Entity::find()
        .filter(memory::Column::ActorId.eq(&row.actor_id))
        .filter(memory::Column::MemoryId.eq(&row.memory_id))
        .filter(memory::Column::Status.ne("forgotten"))
        .one(db)
        .await?;
    Ok(fact.is_some_and(|fact| {
        if fact.device_ref.is_empty() {
            fact.scope_key == "personal" && source_matches_device(origin, target, personal_root)
        } else {
            fact.device_ref == target
                && fact.scope_key == format!("device:{target}")
                && (origin.device_ref.is_empty() || origin.device_ref == target)
        }
    }))
}

/// Native rows and policy already admitted by the host, never model-provided evidence.
pub struct MemoryWriteSource {
    pub origin: origin::Model,
    pub policy: SourceUsePolicy,
    pub body: String,
    pub known: Vec<KnownMemorySource>,
    pub device: Option<AssistantSourceDevice>,
    pub role: SourceRefRole,
}

pub struct MemoryWrite<'a> {
    pub actor: &'a str,
    pub device: &'a str,
    pub instance: Option<&'a str>,
    pub producer_call: &'a str,
    pub tool_call: &'a str,
    pub request_sha256: &'a str,
    pub request: &'a MemoryWriteRequest,
    pub accepted_input_seq: u64,
    pub runtime: &'a runtime::Model,
    pub sources: Vec<MemoryWriteSource>,
    pub production_policy: &'a SourceUsePolicy,
    pub policy: &'a AssistantDataPolicy,
    pub now: u64,
}

pub fn commit_id(actor: &str, call: &str, tool: &str) -> String {
    native_ids::stable(
        Kind::AssistantReadAuthorization,
        &("memory-write", actor, call, tool),
    )
}

pub async fn replay_on<C: ConnectionTrait>(
    db: &C,
    actor: &str,
    call: &str,
    tool: &str,
    request: &str,
) -> Result<Option<MemoryWriteResult>, AssistantStoreError> {
    let Some(row) = receipt::Entity::find()
        .filter(receipt::Column::ActorId.eq(actor))
        .filter(receipt::Column::CommitId.eq(commit_id(actor, call, tool)))
        .one(db)
        .await?
    else {
        return Ok(None);
    };
    if row.request_sha256 != request || row.source_call_id != call {
        return Err(AssistantStoreError::Invalid(
            "memory write replay arguments changed",
        ));
    }
    serde_json::from_str(&row.result_json)
        .map(Some)
        .map_err(|_| AssistantStoreError::Inaccessible)
}

/// Called only after the host has completed its final native/source admission reads.
/// Policy of a draft evidenced only by the owner's own input, or `None` when any source
/// is something else. Each `(origin kind, policy)` is an already loaded source row.
pub fn owner_input_policy<'a>(
    sources: impl Iterator<Item = (&'a str, &'a SourceUsePolicy)>,
) -> Result<Option<SourceUsePolicy>, &'static str> {
    let mut policies = Vec::new();
    for (kind, policy) in sources {
        if kind != "input" || policy.class != SourcePolicyClass::UserText {
            return Ok(None);
        }
        policies.push(policy.clone());
    }
    let Some(first) = policies.first() else {
        return Ok(None);
    };
    desk_diagnose_core::ai_assistant::source::derive_visible_text_policy(
        &policies,
        first.policy_revision,
    )
    .map(Some)
}

pub async fn apply_on<C: ConnectionTrait>(
    db: &C,
    write: MemoryWrite<'_>,
) -> Result<MemoryWriteResult, AssistantStoreError> {
    let Some(primary) = write
        .sources
        .iter()
        .find(|s| s.role == SourceRefRole::Evidence)
    else {
        return Ok(result(
            MemoryWriteOutcome::Skipped,
            "source_unavailable",
            None,
        ));
    };
    let draft = write
        .request
        .draft()
        .ok_or(AssistantStoreError::Inaccessible)?;
    let old = match write.request {
        MemoryWriteRequest::Update {
            memory_id,
            expected_revision,
            ..
        } => {
            let row = memory::Entity::find()
                .filter(memory::Column::ActorId.eq(write.actor))
                .filter(memory::Column::MemoryId.eq(memory_id))
                .one(db)
                .await?;
            let Some(row) = row
                .filter(|row| row.revision == *expected_revision as i64 && row.status == "active")
            else {
                return Ok(result(
                    MemoryWriteOutcome::Conflict,
                    "revision_conflict",
                    None,
                ));
            };
            Some(row)
        }
        MemoryWriteRequest::Remember { .. } => None,
        MemoryWriteRequest::Forget { .. } => return Err(AssistantStoreError::Inaccessible),
    };
    if let Some(previous) = &old {
        let count = revision::Entity::find()
            .filter(revision::Column::ActorId.eq(write.actor))
            .filter(revision::Column::MemoryId.eq(&previous.memory_id))
            .count(db)
            .await?;
        if count >= u64::from(write.policy.max_memory_revisions_per_fact) {
            return Ok(result(
                MemoryWriteOutcome::Rejected,
                "memory_revision_limit",
                None,
            ));
        }
    }
    let (scope_key, device) = match draft.scope {
        MemoryWriteScope::Personal => ("personal".to_owned(), ""),
        MemoryWriteScope::CurrentDevice => (format!("device:{}", write.device), write.device),
    };
    if old
        .as_ref()
        .is_some_and(|row| row.scope_key != scope_key || row.device_ref != device)
    {
        return Ok(result(MemoryWriteOutcome::Rejected, "scope_changed", None));
    }
    let id = old
        .as_ref()
        .map(|row| row.memory_id.clone())
        .unwrap_or_else(|| {
            native_ids::stable(
                Kind::Memory,
                &(write.actor, write.producer_call, write.tool_call),
            )
        });
    let generation = increment(write.runtime.memory_state_generation)?;
    let number = old.as_ref().map_or(Ok(1), |row| increment(row.revision))?;
    let now = signed(write.now)?;
    let next = memory::Model {
        id: old.as_ref().map_or(0, |row| row.id),
        memory_id: id.clone(),
        actor_id: write.actor.into(),
        scope_key,
        device_ref: device.into(),
        body: draft.content.clone(),
        context: draft.context.clone(),
        revision: number,
        state_generation: generation,
        status: "active".into(),
        accepted_input_seq: signed(write.accepted_input_seq)?,
        observed_at_ms: primary.origin.created_at_ms,
        updated_at_ms: now,
        valid_from_ms: primary.origin.created_at_ms,
        valid_until_ms: None,
    };
    let logical = commit_id(write.actor, write.producer_call, write.tool_call);
    let version = revision::Model {
        id: 0,
        memory_id: id.clone(),
        revision: number,
        state_generation: generation,
        actor_id: write.actor.into(),
        producer_call_id: write.producer_call.into(),
        tool_call_id: write.tool_call.into(),
        logical_mutation_id: logical.clone(),
        write_outcome: "applied".into(),
        body: next.body.clone(),
        context: next.context.clone(),
        status: "active".into(),
        accepted_input_seq: next.accepted_input_seq,
        dependencies_json: "[]".into(),
        created_at_ms: now,
        expires_at_ms: None,
    };
    let mut sources = Vec::new();
    for input in &write.sources {
        let policy = desk_diagnose_core::ai_assistant::source::derive_visible_text_policy(
            &[input.policy.clone(), write.production_policy.clone()],
            input.policy.policy_revision,
        )
        .map_err(AssistantStoreError::Invalid)?;
        let kind = match input.origin.kind.as_str() {
            "input" => AssistantSourceKind::Input,
            "receipt" => AssistantSourceKind::Receipt,
            "native_status" => AssistantSourceKind::NativeStatus,
            "assistant_message" => AssistantSourceKind::AssistantMessage,
            _ => return Err(AssistantStoreError::Inaccessible),
        };
        let forget_sources = input
            .known
            .iter()
            .filter(|source| source.origin_id == input.origin.origin_id)
            .cloned()
            .collect::<Vec<_>>();
        let proof = MemoryEvidenceRetentionProof::from_snapshot(&MemoryEvidenceSnapshot {
            actor_id: write.actor,
            memory_id: &id,
            memory_revision: unsigned(number)?,
            memory_accepted_input_seq: unsigned(next.accepted_input_seq)?,
            role: input.role,
            body: &input.body,
            origin_id: &input.origin.origin_id,
            origin_kind: kind,
            origin_deletion_generation: unsigned(input.origin.deletion_generation)?,
            origin_accepted_input_seq: unsigned(input.origin.accepted_input_seq)?,
            recorded_at_unix_ms: unsigned(input.origin.created_at_ms)?,
            statement_timezone: &input.origin.statement_timezone,
            origin_policy: &input.policy,
            evidence_policy: &policy,
            known_sources: &input.known,
            forget_sources: &forget_sources,
            production_dependencies: &[],
            retained_origins: &[],
        })
        .map_err(AssistantStoreError::Invalid)?;
        sources.push(source::Model {
            id: 0,
            memory_id: id.clone(),
            memory_revision: number,
            actor_id: write.actor.into(),
            origin_id: input.origin.origin_id.clone(),
            role: match input.role {
                SourceRefRole::Evidence => "evidence",
                SourceRefRole::Background => "background",
                SourceRefRole::PreviousMemory => "previous_memory",
            }
            .into(),
            body: input.body.clone(),
            retained_bytes: i32::try_from(input.body.len())
                .map_err(|_| AssistantStoreError::Capacity)?,
            body_cleared_at_ms: None,
            dependencies_json: "[]".into(),
            known_sources_json: json(&input.known)?,
            forget_sources_json: json(&forget_sources)?,
            retention_proof_json: json(&proof)?,
            policy_json: json(&policy)?,
            source_device_json: input.device.as_ref().map(json).transpose()?,
            observed_at_ms: input.origin.created_at_ms,
            statement_timezone: input.origin.statement_timezone.clone(),
            revoked_at_ms: None,
            expires_at_ms: proof.hard_expires_at_unix_ms.map(signed).transpose()?,
        });
    }
    let text = format!("{}\n{}", next.body, next.context.as_deref().unwrap_or(""));
    let index = memory_index::prepare_on(
        db,
        memory_index::MemoryIndexInput {
            next: &next,
            removed: old.iter().map(|row| row.memory_id.clone()).collect(),
            text: &text,
            source_origin_id: &primary.origin.origin_id,
            origin: &primary.origin,
            policy: write.policy,
            body_expiry: write
                .production_policy
                .content_hard_expires_at_unix_ms
                .map(signed)
                .transpose()?,
        },
    )
    .await?;
    let mut delta = index.delta;
    delta.memory_records = i64::from(old.is_none());
    delta.memory_body_bytes += quota::memory_body(&next)? + quota::version_body(&version)?
        - old
            .as_ref()
            .map(quota::memory_body)
            .transpose()?
            .unwrap_or(0);
    delta.metadata_bytes += quota::version_metadata(&version)?
        + if old.is_none() {
            quota::memory_metadata(&next)?
        } else {
            0
        };
    for row in &sources {
        delta.memory_body_bytes += i64::from(row.retained_bytes);
        delta.metadata_bytes += quota::source_metadata(row)?;
    }
    if let Some(old) = &old {
        let mut active: memory::ActiveModel = next.clone().into();
        active = active.reset_all();
        active.id = ActiveValue::NotSet;
        let changed = memory::Entity::update_many()
            .set(active)
            .filter(memory::Column::Id.eq(old.id))
            .filter(memory::Column::ActorId.eq(write.actor))
            .filter(memory::Column::Revision.eq(old.revision))
            .exec(db)
            .await?;
        if changed.rows_affected != 1 {
            return Err(AssistantStoreError::Inaccessible);
        }
        let expiry = write
            .now
            .checked_add(u64::from(write.policy.old_memory_revision_days) * 86_400_000)
            .ok_or(AssistantStoreError::Capacity)?;
        revision::Entity::update_many()
            .col_expr(revision::Column::ExpiresAtMs, Expr::value(signed(expiry)?))
            .filter(revision::Column::ActorId.eq(write.actor))
            .filter(revision::Column::MemoryId.eq(&id))
            .filter(revision::Column::Revision.eq(old.revision))
            .exec(db)
            .await?;
    } else {
        let mut active: memory::ActiveModel = next.clone().into();
        active.id = ActiveValue::NotSet;
        active.insert(db).await?;
    }
    let mut active: revision::ActiveModel = version.into();
    active.id = ActiveValue::NotSet;
    active.insert(db).await?;
    for row in sources {
        let mut active: source::ActiveModel = row.into();
        active.id = ActiveValue::NotSet;
        active.insert(db).await?;
    }
    memory_index::apply_on(db, index).await?;
    let changed = runtime::Entity::update_many()
        .col_expr(
            runtime::Column::MemoryStateGeneration,
            Expr::value(generation),
        )
        .col_expr(
            runtime::Column::Revision,
            Expr::value(increment(write.runtime.revision)?),
        )
        .filter(runtime::Column::ActorId.eq(write.actor))
        .filter(runtime::Column::Revision.eq(write.runtime.revision))
        .filter(runtime::Column::ControlGeneration.eq(write.runtime.control_generation))
        .filter(runtime::Column::LearningCutoffInputSeq.eq(write.runtime.learning_cutoff_input_seq))
        .exec(db)
        .await?;
    if changed.rows_affected != 1 {
        return Err(AssistantStoreError::Inaccessible);
    }
    let value = result(
        MemoryWriteOutcome::Applied,
        "committed",
        Some(MemoryWritten {
            memory_id: id,
            revision: unsigned(number)?,
        }),
    );
    let result_json = json(&value)?;
    let commit = receipt::Model {
        id: 0,
        commit_id: logical,
        actor_id: write.actor.into(),
        instance_id: write.instance.map(str::to_owned),
        source_call_id: write.producer_call.into(),
        request_sha256: write.request_sha256.into(),
        memory_state_generation: generation,
        result_json,
        created_at_ms: now,
        expires_at_ms: signed(
            write
                .now
                .checked_add(u64::from(write.policy.history_retention_days) * 86_400_000)
                .ok_or(AssistantStoreError::Capacity)?,
        )?,
    };
    delta.metadata_bytes += receipts::metadata_bytes(&commit)?;
    let mut active: receipt::ActiveModel = commit.into();
    active.id = ActiveValue::NotSet;
    active.insert(db).await?;
    quota::apply_on(db, write.actor, delta, write.policy).await?;
    Ok(value)
}

pub fn result(
    outcome: MemoryWriteOutcome,
    reason: &str,
    memory: Option<MemoryWritten>,
) -> MemoryWriteResult {
    MemoryWriteResult {
        outcome,
        memory,
        reason: Some(reason.into()),
        control_receipt: None,
    }
}
fn json<T: serde::Serialize>(value: &T) -> Result<String, AssistantStoreError> {
    serde_json::to_string(value).map_err(|_| AssistantStoreError::Inaccessible)
}

#[cfg(test)]
mod owner_input_policy_tests {
    use super::*;
    use desk_diagnose_core::ai_assistant::source::owner_text_policy;

    #[test]
    fn only_owner_input_evidence_supplies_a_policy() {
        let owner = owner_text_policy(3, false);
        let policy = owner_input_policy([("input", &owner)].into_iter())
            .unwrap()
            .unwrap();
        assert!(desk_diagnose_core::ai_assistant::memory_evidence::retained_text_policy(&policy));
        assert_eq!(policy.policy_revision, 3);

        // Assistant text citing mixed sources stays under the whole-context rule.
        assert_eq!(
            owner_input_policy([("input", &owner), ("assistant_message", &owner)].into_iter()),
            Ok(None)
        );
        assert_eq!(owner_input_policy(std::iter::empty()), Ok(None));
        let mut device = owner.clone();
        device.class = SourcePolicyClass::StructuredObservation;
        assert_eq!(
            owner_input_policy([("input", &device)].into_iter()),
            Ok(None)
        );
    }
}
