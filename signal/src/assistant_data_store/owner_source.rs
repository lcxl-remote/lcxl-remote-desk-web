//! Authenticated owner source viewing never grants permission for a model send.

use super::{AssistantStoreError, ensure_runtime_on, unsigned};
use crate::{
    config::ConfigConnection,
    entity::{
        agent_assistant_history as history, agent_assistant_origin as origin,
        agent_assistant_search_document as document, agent_memory as memory,
        agent_memory_revision as revision, agent_memory_source as evidence,
    },
};
use desk_agent_protocol::ai_assistant::{
    source::{
        AssistantSourceLocator, AssistantSourceResolution, HistoryDependency, SourceAvailability,
        SourcePolicyClass, SourceUsePolicy,
    },
    user_instructions::AssistantDataPolicy,
};
use desk_diagnose_core::{
    ai_assistant::source::{HistoryRetentionProof, validate_policy},
    native_ids::{self, Kind},
};
use sea_orm::{
    ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, QuerySelect, TransactionTrait,
};
use sha2::{Digest, Sha256};

pub(crate) async fn owner_source(
    db: &crate::config::connection::DatabaseConnection,
    source_id: &str,
    now: u64,
) -> Result<AssistantSourceResolution, AssistantStoreError> {
    if !native_ids::is(Kind::AssistantSource, source_id) {
        return Err(AssistantStoreError::Invalid(
            "invalid assistant source identity",
        ));
    }
    let txn = db.begin().await?;
    let actor = crate::control_authorizer::SINGLE_ACCOUNT_USER_ID.to_string();
    ensure_runtime_on(&txn, &actor).await?;
    let config = txn.config_read().await;
    let result =
        owner_source_on(&txn, &actor, source_id, &config.assistant_data_policy, now).await?;
    txn.commit().await?;
    Ok(result)
}

pub(crate) async fn owner_source_on<C: ConnectionTrait + ConfigConnection>(
    db: &C,
    actor: &str,
    source_id: &str,
    platform_policy: &AssistantDataPolicy,
    now: u64,
) -> Result<AssistantSourceResolution, AssistantStoreError> {
    if let Some(row) = history::Entity::find()
        .filter(history::Column::ActorId.eq(actor))
        .filter(history::Column::SourceId.eq(source_id))
        .one(db)
        .await?
    {
        return history_source_on(db, actor, &row, platform_policy, now).await;
    }
    owner_memory_source_on(db, actor, source_id, platform_policy, now).await
}

/// Owner navigation may resolve the immutable original fragment after the
/// independent memory body is revoked. Model evidence readers never enter here.
async fn owner_memory_source_on<C: ConnectionTrait + ConfigConnection>(
    db: &C,
    actor: &str,
    source_id: &str,
    policy: &AssistantDataPolicy,
    now: u64,
) -> Result<AssistantSourceResolution, AssistantStoreError> {
    if let Some(source) = evidence::Entity::find()
        .filter(evidence::Column::ActorId.eq(actor))
        .filter(evidence::Column::SourceId.eq(source_id))
        .one(db)
        .await?
        && super::evidence::owner_history_identity_allowed_on(db, actor, &source, now).await?
    {
        // Use actual cited fragment keys, never all producer dependencies or a
        // first search hit. An ambiguous original is not a navigation target.
        let keys = super::evidence::forgetting_sources(&source)?;
        let primary = keys
            .iter()
            .filter(|key| key.origin_id == source.origin_id)
            .collect::<Vec<_>>();
        if let [key] = primary.as_slice()
            && let Some(original) = history::Entity::find()
                .filter(history::Column::ActorId.eq(actor))
                .filter(history::Column::SourceId.eq(&key.source_id))
                .filter(history::Column::OriginId.eq(&key.origin_id))
                .one(db)
                .await?
            && unsigned(original.revision)? == key.revision
            && original.content_sha256 == key.content_sha256
        {
            let mut resolved = history_source_on(db, actor, &original, policy, now).await?;
            if matches!(
                resolved.availability,
                SourceAvailability::Available
                    | SourceAvailability::Partial
                    | SourceAvailability::Redacted
            ) {
                let prior = revision::Entity::find()
                    .filter(revision::Column::ActorId.eq(actor))
                    .filter(revision::Column::MemoryId.eq(&source.memory_id))
                    .filter(revision::Column::Revision.eq(source.memory_revision))
                    .one(db)
                    .await?;
                resolved.source_id = source.source_id;
                resolved.revision = unsigned(source.memory_revision)?;
                resolved.notice = Some(
                    if prior.is_some_and(|value| value.status == "forgotten") {
                        "memory_forgotten_original_kept"
                    } else {
                        "original_history_available"
                    }
                    .into(),
                );
                return Ok(resolved);
            }
        }
    }
    // This retains the independent-evidence lifetime and all existing source,
    // memory version and model-purpose restrictions when no original is usable.
    memory_source_on(db, actor, source_id, policy, now).await
}

/// Model callers authorize the exact evidence purpose separately. They must
/// not fall back to a same-ID history row with a different source policy.
pub(super) async fn memory_source_on<C: ConnectionTrait + ConfigConnection>(
    db: &C,
    actor: &str,
    source_id: &str,
    platform_policy: &AssistantDataPolicy,
    now: u64,
) -> Result<AssistantSourceResolution, AssistantStoreError> {
    let Some(source) = evidence::Entity::find()
        .filter(evidence::Column::ActorId.eq(actor))
        .filter(evidence::Column::SourceId.eq(source_id))
        .one(db)
        .await?
    else {
        return Ok(unavailable(source_id));
    };
    let Some(fact) = memory::Entity::find()
        .filter(memory::Column::ActorId.eq(actor))
        .filter(memory::Column::MemoryId.eq(&source.memory_id))
        .one(db)
        .await?
    else {
        return Ok(unavailable(source_id));
    };
    if fact.status == "forgotten"
        || source.memory_revision > fact.revision
        || !super::memory::evidence_allowed_on(db, actor, &source, now).await?
    {
        return Ok(unavailable(source_id));
    }
    if source.memory_revision != fact.revision {
        let prior = revision::Entity::find()
            .filter(revision::Column::ActorId.eq(actor))
            .filter(revision::Column::MemoryId.eq(&fact.memory_id))
            .filter(revision::Column::Revision.eq(source.memory_revision))
            .one(db)
            .await?;
        if !prior.is_some_and(|row| {
            row.status != "forgotten"
                && row
                    .expires_at_ms
                    .is_some_and(|until| until > 0 && until as u64 > now)
        }) {
            return Ok(unavailable(source_id));
        }
    }
    let origin = origin::Entity::find()
        .filter(origin::Column::ActorId.eq(actor))
        .filter(origin::Column::OriginId.eq(&source.origin_id))
        .one(db)
        .await?
        .ok_or(AssistantStoreError::Inaccessible)?;
    let device = super::memory::source_device_on(db, actor, &origin.device_ref).await?;
    let original_deadline = unsigned(origin.created_at_ms)?
        .saturating_add(u64::from(platform_policy.history_retention_days) * 86_400_000);
    let notice = if !origin.history_admitted {
        "original_fragment_not_retained"
    } else if original_deadline <= now {
        "original_history_expired"
    } else {
        "original_fragment_unavailable"
    };
    Ok(AssistantSourceResolution {
        source_id: source_id.into(),
        revision: unsigned(source.memory_revision)?,
        availability: SourceAvailability::EvidenceOnly,
        locator: None,
        source_device: device,
        content: Some(source.body),
        recorded_at_unix_ms: Some(unsigned(source.observed_at_ms)?),
        statement_timezone: Some(source.statement_timezone),
        notice: Some(notice.into()),
    })
}

pub(super) async fn history_source_on<C: ConnectionTrait + ConfigConnection>(
    db: &C,
    actor: &str,
    row: &history::Model,
    platform_policy: &AssistantDataPolicy,
    now: u64,
) -> Result<AssistantSourceResolution, AssistantStoreError> {
    let current_expiry = unsigned(row.recorded_at_ms)?
        .saturating_add(u64::from(platform_policy.history_retention_days) * 86_400_000);
    if row.deleted_at_ms.is_some()
        || row.expires_at_ms <= 0
        || row.expires_at_ms as u64 <= now
        || current_expiry <= now
        || row.revision <= 0
        || row.content.len() > 4096
        || row.content_sha256 != format!("{:x}", Sha256::digest(row.content.as_bytes()))
    {
        return Ok(unavailable(&row.source_id));
    }
    let Some(original) = origin::Entity::find()
        .filter(origin::Column::ActorId.eq(actor))
        .filter(origin::Column::OriginId.eq(&row.origin_id))
        .one(db)
        .await?
    else {
        return Ok(unavailable(&row.source_id));
    };
    let device = super::memory::source_device_on(db, actor, &row.device_ref).await?;
    if !super::source_lifecycle::origin_readable_on(db, actor, &original).await?
        || !original.history_admitted
        || original.device_ref != row.device_ref
        || (!row.device_ref.is_empty() && device.is_none())
        || original.conversation_id != row.conversation_id
        || original.root_conversation_id != row.root_conversation_id
        || original.native_id != row.native_id
        || original.kind != row.kind
    {
        return Ok(unavailable(&row.source_id));
    }
    let Ok(policy) = serde_json::from_str::<SourceUsePolicy>(&original.policy_json) else {
        return Ok(unavailable(&row.source_id));
    };
    let Ok(proof) = serde_json::from_str::<HistoryRetentionProof>(&row.retention_proof_json) else {
        return Ok(unavailable(&row.source_id));
    };
    let native_without_owner_input = match super::history_native::read(row, &proof) {
        Ok(native) => super::history_native::zero_input_allowed(row, native.as_ref()),
        Err(_) => return Ok(unavailable(&row.source_id)),
    };
    if !history_policy_allowed(&policy, now)
        || proof.actor_id != actor
        || proof.origin_id != row.origin_id
        || (proof.accepted_input_seq == 0 && !native_without_owner_input)
        || proof.accepted_input_seq != unsigned(original.accepted_input_seq)?
        || proof.device_ref != row.device_ref
        || proof.original_content_sha256 != original.original_sha256
        || proof.source_policy_sha256 != policy.policy_sha256
        || proof.policy_revision != policy.policy_revision
        || proof.recorded_at_unix_ms != unsigned(row.recorded_at_ms)?
        || proof.statement_timezone != row.statement_timezone
        || original.statement_timezone != row.statement_timezone
        || !desk_diagnose_core::ai_assistant::source::valid_statement_timezone(
            &row.statement_timezone,
        )
        || proof.expires_at_unix_ms != unsigned(row.expires_at_ms)?
    {
        return Ok(unavailable(&row.source_id));
    }
    let Ok(dependencies) = serde_json::from_str::<Vec<HistoryDependency>>(&row.dependencies_json)
    else {
        return Ok(unavailable(&row.source_id));
    };
    if desk_diagnose_core::ai_assistant::source::history_dependencies_sha256(&dependencies)
        .ok()
        .as_deref()
        != Some(proof.production_dependencies_sha256.as_str())
    {
        return Ok(unavailable(&row.source_id));
    }
    for dependency in dependencies {
        if dependency
            .expires_at_unix_ms
            .is_some_and(|until| until <= now)
        {
            return Ok(unavailable(&row.source_id));
        }
        let Some(origin) = origin::Entity::find()
            .filter(origin::Column::ActorId.eq(actor))
            .filter(origin::Column::OriginId.eq(&dependency.origin_id))
            .one(db)
            .await?
        else {
            return Ok(unavailable(&row.source_id));
        };
        let Ok(policy) = serde_json::from_str::<SourceUsePolicy>(&origin.policy_json) else {
            return Ok(unavailable(&row.source_id));
        };
        let source_policy_revision = if let Some(source) = evidence::Entity::find()
            .filter(evidence::Column::ActorId.eq(actor))
            .filter(evidence::Column::SourceId.eq(&dependency.source_id))
            .one(db)
            .await?
        {
            let Ok(source_policy) = serde_json::from_str::<SourceUsePolicy>(&source.policy_json)
            else {
                return Ok(unavailable(&row.source_id));
            };
            if source.origin_id != dependency.origin_id
                || unsigned(source.memory_revision)? != dependency.source_revision
                || source.content_sha256 != dependency.content_sha256
                || !history_policy_allowed(&source_policy, now)
                || !super::evidence::owner_history_identity_allowed_on(db, actor, &source, now)
                    .await?
            {
                return Ok(unavailable(&row.source_id));
            }
            source_policy.policy_revision
        } else {
            policy.policy_revision
        };
        if !super::source_lifecycle::origin_readable_on(db, actor, &origin).await?
            || unsigned(origin.deletion_generation)? != dependency.origin_deletion_generation
            || source_policy_revision != dependency.policy_revision
            || !history_policy_allowed(&policy, now)
            || (!origin.device_ref.is_empty()
                && super::memory::source_device_on(db, actor, &origin.device_ref)
                    .await?
                    .is_none())
        {
            return Ok(unavailable(&row.source_id));
        }
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
    if docs.len() != 1 {
        return Ok(unavailable(&row.source_id));
    }
    let doc = &docs[0];
    if doc.byte_start < 0
        || doc.byte_end < doc.byte_start
        || doc.byte_end - doc.byte_start != row.content.len() as i32
        || doc.origin_id != row.origin_id
        || doc.device_ref != row.device_ref
        || doc.conversation_id != row.conversation_id
        || doc.root_conversation_id != row.root_conversation_id
        || doc.fragment_index != row.fragment_index
        || doc.expires_at_ms != Some(row.expires_at_ms)
        || unsigned(doc.policy_revision)? != policy.policy_revision
    {
        return Ok(unavailable(&row.source_id));
    }
    let kind = row.kind.as_str();
    let message_id = if matches!(kind, "input" | "assistant_message") {
        Some(row.native_id.clone())
    } else if kind == "receipt" {
        original_result_message_on(db, actor, row, &proof.original_envelope_id).await?
    } else {
        None
    };
    Ok(AssistantSourceResolution {
        source_id: row.source_id.clone(),
        revision: unsigned(row.revision)?,
        availability: if doc.partial {
            SourceAvailability::Partial
        } else if row.admission == "redacted" {
            SourceAvailability::Redacted
        } else {
            SourceAvailability::Available
        },
        locator: Some(AssistantSourceLocator {
            conversation_id: row.conversation_id.clone(),
            root_conversation_id: row.root_conversation_id.clone(),
            message_id,
            input_event_id: None,
            action_id: None,
            result_id: (kind == "receipt").then(|| proof.original_envelope_id.clone()),
            content_sha256: row.content_sha256.clone(),
            fragment_index: u32::try_from(row.fragment_index)
                .map_err(|_| AssistantStoreError::Invalid("invalid source fragment"))?,
            byte_start: doc.byte_start as u32,
            byte_end: doc.byte_end as u32,
        }),
        source_device: device,
        content: Some(row.content.clone()),
        recorded_at_unix_ms: Some(unsigned(row.recorded_at_ms)?),
        statement_timezone: Some(row.statement_timezone.clone()),
        notice: None,
    })
}

/// A receipt index ID is not a chat message ID. Resolve only its exact original
/// result envelope in the original fully decoded main-assistant session.
pub(super) async fn original_result_message_on<C: ConnectionTrait>(
    db: &C,
    actor: &str,
    source: &history::Model,
    envelope_id: &str,
) -> Result<Option<String>, AssistantStoreError> {
    use crate::entity::agent_session;
    use desk_diagnose_core::session::PersistedAgentSession;
    let Some(row) = agent_session::Entity::find()
        .filter(agent_session::Column::ActorId.eq(actor))
        .filter(agent_session::Column::ConversationId.eq(&source.conversation_id))
        .one(db)
        .await?
    else {
        return Ok(None);
    };
    let Ok(session) = PersistedAgentSession::decode_json(&row.state_json) else {
        return Ok(None);
    };
    if session.actor_id != actor
        || session.conversation_id != source.conversation_id
        || !desk_diagnose_core::ai_assistant::data_access::eligible_origin(&session)
        || session
            .assistant_data
            .as_ref()
            .is_none_or(|data| data.source_device_ref != source.device_ref)
    {
        return Ok(None);
    }
    Ok(
        desk_diagnose_core::ai_assistant::native_history::original_result_message_id(
            &session.conversation,
            envelope_id,
        ),
    )
}

fn history_policy_allowed(policy: &SourceUsePolicy, now: u64) -> bool {
    validate_policy(policy).is_ok()
        && policy.history_retention_allowed
        && !policy.no_copy
        && !matches!(
            policy.class,
            SourcePolicyClass::Media
                | SourcePolicyClass::ProviderPrivate
                | SourcePolicyClass::Unknown
        )
        && policy
            .content_hard_expires_at_unix_ms
            .is_none_or(|until| until > now)
}

fn unavailable(source_id: &str) -> AssistantSourceResolution {
    AssistantSourceResolution {
        source_id: source_id.into(),
        revision: 0,
        availability: SourceAvailability::Unavailable,
        locator: None,
        source_device: None,
        content: None,
        recorded_at_unix_ms: None,
        statement_timezone: None,
        notice: None,
    }
}
