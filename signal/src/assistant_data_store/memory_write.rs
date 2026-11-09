//! Admit one native tool invocation, then synchronously commit the model's memory draft.

use super::{AssistantStoreError, signed};
use crate::config::connection::{DatabaseConnection, DatabaseTransaction};
use crate::entity::agent_assistant_history as history;
use crate::entity::agent_assistant_origin as origin;
use desk_agent_protocol::ai_assistant::{memory::*, source::*};
use desk_assistant_data_store::memory_write::{self as shared, MemoryWrite, MemoryWriteSource};
use desk_diagnose_core::ai_assistant::seam::{AssistantDataCall, AssistantDataRead};
use desk_diagnose_core::{
    ai_assistant::memory_access::{request_memory_target, usable_memory_scope},
    session::PersistedAgentSession,
};
use sea_orm::{
    ColumnTrait, EntityTrait, QueryFilter, Set, TransactionTrait, sea_query::OnConflict,
};

pub(crate) async fn write(
    db: &DatabaseConnection,
    context: AssistantDataCall<'_>,
    request: &MemoryWriteRequest,
) -> Result<AssistantDataRead<MemoryWriteResult>, AssistantStoreError> {
    let txn = db.begin().await?;
    let result = desk_diagnose_core::future::send_phase(|| write_on(&txn, context, request)).await;
    match result {
        Ok(result) => {
            txn.commit().await?;
            Ok(AssistantDataRead {
                result,
                projection: None,
            })
        }
        Err(error) => {
            txn.rollback().await?;
            Err(error)
        }
    }
}

async fn write_on(
    db: &DatabaseTransaction,
    context: AssistantDataCall<'_>,
    request: &MemoryWriteRequest,
) -> Result<MemoryWriteResult, AssistantStoreError> {
    let now = super::conversation_search::now_ms()?;
    let context = AssistantDataCall {
        now_unix_ms: now,
        ..context
    };
    let fence = desk_diagnose_core::future::send_phase(|| {
        super::retrieval_native::prepare_memory_write_on(db, context, now)
    })
    .await?;
    let archive = fence
        .producer_archive()
        .ok_or(AssistantStoreError::Inaccessible)?;
    let call = &archive.proof.source_set.physical_call_id;
    let digest = desk_diagnose_core::ai_assistant::read_projection::request_sha256(
        &context.tool_call.name,
        &context.tool_call.arguments_json,
    )
    .map_err(AssistantStoreError::Invalid)?;
    let data = context
        .session
        .assistant_data
        .as_ref()
        .ok_or(AssistantStoreError::Inaccessible)?;
    if let Some(value) = shared::replay_on(
        db,
        &context.session.actor_id,
        call,
        &context.tool_call.id,
        &digest,
    )
    .await?
    {
        fence.finish_on(now).await?;
        if fence.access.control_generation != data.control_generation
            || !fence.access.settings.automatic_memory_enabled
        {
            return Ok(skipped("source_changed"));
        }
        return Ok(value);
    }
    if let Some(reason) = fence.write_skip_reason() {
        return Ok(skipped(reason));
    }
    let recovered = if archive.proof.retained_policy.is_none() {
        super::visible_history::native::resolve_policy_on(db, &archive.proof, now).await?
    } else {
        None
    };
    let context_policy = archive
        .proof
        .retained_policy
        .as_ref()
        .or_else(|| recovered.as_ref().map(|retained| &retained.policy))
        .filter(|policy| {
            desk_diagnose_core::ai_assistant::memory_evidence::retained_text_policy(policy)
        });
    if context_policy.is_some_and(|policy| {
        policy
            .content_hard_expires_at_unix_ms
            .is_some_and(|until| until <= now)
    }) {
        return Ok(skipped("content_expired"));
    }

    let labels = archive.proof.sources.input_snapshot.policy_sources();
    if let MemoryWriteRequest::Update {
        memory_id,
        expected_revision,
        ..
    } = request
    {
        let mut observed = false;
        for (_, read) in
            super::native_export::load_call_set_with_projections_on(db, &archive.proof.source_set)
                .await?
        {
            observed |= read
                .proof
                .usage
                .memories
                .iter()
                .any(|m| m.memory_id == *memory_id && m.revision == *expected_revision);
        }
        for label in &labels {
            if label.provenance.source_provider_id == "assistant-conversation-snapshot" {
                let read = super::projection::load_conversation_snapshot_on(
                    db,
                    &context.session.actor_id,
                    &context.session.conversation_id,
                    &data.source_device_ref,
                    label,
                )
                .await?;
                observed |= read
                    .proof
                    .usage
                    .memories
                    .iter()
                    .any(|m| m.memory_id == *memory_id && m.revision == *expected_revision);
            }
        }
        if !observed {
            return Ok(skipped("target_not_observed"));
        }
    }
    let state = super::runtime_on(db, &context.session.actor_id).await?;
    let draft = request.draft().ok_or(AssistantStoreError::Inaccessible)?;
    let Some((target_device, audience)) =
        desk_diagnose_core::future::boxed(|| resolve_device_on(db, context.session, &draft, now))
            .await?
    else {
        return Ok(skipped("source_out_of_scope"));
    };
    let personal_root = context
        .session
        .has_admitted_personal_input()
        .then_some(context.session.conversation_id.as_str());
    let mut refs = draft.source_refs.clone();
    // Earlier owner inputs are named `input:<n>` relative to the model call;
    // resolve them before binding the current input so duplicates collapse.
    for reference in &mut refs {
        if !reference
            .id
            .starts_with(desk_diagnose_core::ai_assistant::data_tools::OWNER_INPUT_REF_PREFIX)
        {
            continue;
        }
        let message = desk_diagnose_core::ai_assistant::data_tools::owner_input_message(
            context.session,
            &context.producer_message.message_id,
            &reference.id,
        )
        .ok_or(AssistantStoreError::SourceUnavailable)?;
        let origin_id = desk_diagnose_core::ai_assistant::source::history_origin_id(
            &context.session.actor_id,
            &data.source_device_ref,
            &context.session.conversation_id,
            &message.message_id,
            AssistantSourceKind::Input,
        );
        reference.id = input_source_id_on(db, &context.session.actor_id, &origin_id)
            .await?
            .ok_or(AssistantStoreError::SourceUnavailable)?;
    }
    if data.current_actor_input_seq > 0 {
        let input = origin::Entity::find()
            .filter(origin::Column::ActorId.eq(&context.session.actor_id))
            .filter(origin::Column::RootConversationId.eq(&context.session.conversation_id))
            .filter(origin::Column::AcceptedInputSeq.eq(signed(data.current_actor_input_seq)?))
            .filter(origin::Column::Kind.eq("input"))
            .one(db)
            .await?;
        let id = match input {
            Some(input) => {
                input_source_id_on(db, &context.session.actor_id, &input.origin_id).await?
            }
            None => None,
        };
        if let Some(id) = id
            && !refs.iter().any(|r| r.id == id)
        {
            refs.push(AssistantSourceRef {
                kind: AssistantSourceKind::Input,
                id,
                revision: None,
                role: SourceRefRole::Evidence,
            });
        }
    }
    if refs.len() > data.limits.memory.source_refs as usize {
        return Err(AssistantStoreError::Capacity);
    }
    let mut sources = Vec::new();

    let mut originals: Vec<shared::sources::MemorySourceRows> = Vec::new();
    let mut retained = Vec::new();
    for reference in refs {
        if originals
            .iter()
            .any(|row| row.history.source_id() == reference.id)
        {
            continue;
        }
        let loaded = shared::sources::load_on(
            db,
            context,
            &reference,
            &labels,
            fence.observed_version(&reference.id),
            &state,
        )
        .await;
        let source = match loaded {
            Ok(Some(source)) => source,
            Ok(None) => {
                let Some(row) = crate::entity::agent_memory_source::Entity::find()
                    .filter(
                        crate::entity::agent_memory_source::Column::ActorId
                            .eq(&context.session.actor_id),
                    )
                    .filter(crate::entity::agent_memory_source::Column::Id.eq(
                        desk_assistant_data_store::source_rows::memory(&reference.id),
                    ))
                    .one(db)
                    .await?
                else {
                    return Ok(skipped("source_unavailable"));
                };
                if fence
                    .observed_version(&reference.id)
                    .is_none_or(|seen| seen.revision != row.memory_revision as u64)
                {
                    return Ok(skipped("source_not_observed"));
                }
                let Some(metadata) =
                    super::evidence::load_on(db, &context.session.actor_id, &row, false, None, now)
                        .await?
                else {
                    return Ok(skipped("source_unavailable"));
                };
                let device = if metadata.primary.device_ref.is_empty() {
                    None
                } else {
                    let Some(device) = super::memory::source_device_on(
                        db,
                        &context.session.actor_id,
                        &metadata.primary.device_ref,
                    )
                    .await?
                    else {
                        return Ok(skipped("source_unavailable"));
                    };
                    Some(device)
                };
                if !desk_diagnose_core::future::boxed(|| {
                    shared::retained_source_matches_device_on(
                        db,
                        &row,
                        &metadata.primary,
                        &target_device,
                        personal_root,
                    )
                })
                .await?
                {
                    return Ok(skipped("source_out_of_scope"));
                }
                sources.push(MemoryWriteSource {
                    origin: metadata.primary,
                    policy: metadata.primary_policy,
                    body: row.body.clone(),
                    known: metadata.known,
                    device,
                    role: SourceRefRole::Background,
                });
                retained.push(row);
                continue;
            }
            Err(AssistantStoreError::SourceUnavailable) => {
                return Ok(skipped("source_unavailable"));
            }
            Err(error) => return Err(error),
        };
        if !super::source_lifecycle::origin_readable_on(
            db,
            &context.session.actor_id,
            &source.origin,
        )
        .await?
        {
            return Ok(skipped("source_unavailable"));
        }
        let device = if source.origin.device_ref.is_empty() {
            None
        } else {
            let Some(device) = super::memory::source_device_on(
                db,
                &context.session.actor_id,
                &source.origin.device_ref,
            )
            .await?
            else {
                return Ok(skipped("source_unavailable"));
            };
            Some(device)
        };
        if !shared::source_matches_device(&source.origin, &target_device, personal_root) {
            return Ok(skipped("source_out_of_scope"));
        }
        sources.push(source.snapshot(device, data.current_actor_input_seq));
        originals.push(source);
    }
    // A draft evidenced only by the owner's own input does not inherit restrictions
    // from unrelated tool results in the same model context.
    let owner_policy = (context_policy.is_none() && retained.is_empty())
        .then(|| {
            shared::owner_input_policy(
                originals
                    .iter()
                    .map(|source| (source.origin.kind.as_str(), &source.policy)),
            )
        })
        .transpose()
        .map_err(AssistantStoreError::Invalid)?
        .flatten();
    let Some(production) = context_policy.or(owner_policy.as_ref()) else {
        return Ok(skipped("content_retention_denied"));
    };
    for source in &originals {
        source.validate_on(db).await?;
    }
    for source in &retained {
        if crate::entity::agent_memory_source::Entity::find_by_id(source.id)
            .one(db)
            .await?
            .as_ref()
            != Some(source)
        {
            return Err(AssistantStoreError::Inaccessible);
        }
    }
    fence
        .finish_on(super::conversation_search::now_ms()?)
        .await?;
    let result = shared::apply_on(
        db,
        MemoryWrite {
            actor: &context.session.actor_id,
            device: &target_device,
            instance: data.source_instance_id.as_deref(),
            producer_call: call,
            tool_call: &context.tool_call.id,
            request_sha256: &digest,
            request,
            accepted_input_seq: data.current_actor_input_seq,
            runtime: &state,
            sources,
            production_policy: production,
            policy: &fence.policy,
            now,
        },
    )
    .await?;
    if result.outcome == MemoryWriteOutcome::Applied
        && let Some(audience) = audience
    {
        let instance = data
            .source_instance_id
            .as_deref()
            .ok_or(AssistantStoreError::Inaccessible)?;
        desk_diagnose_core::future::boxed(|| {
            ensure_device_binding_on(db, &target_device, instance, &audience, now)
        })
        .await?;
    }
    Ok(result)
}

async fn resolve_device_on(
    db: &DatabaseTransaction,
    session: &PersistedAgentSession,
    draft: &MemoryDraft,
    now: u64,
) -> Result<Option<(String, Option<String>)>, AssistantStoreError> {
    if draft.scope == MemoryWriteScope::Personal {
        return Ok(Some((String::new(), None)));
    }
    let Some(scope) = usable_memory_scope(session, now) else {
        return Ok(None);
    };
    if !session.device_id.is_empty() {
        return Ok(draft
            .target_ref
            .as_deref()
            .is_none_or(|reference| reference == session.device_id)
            .then(|| (scope.current_device_ref.clone(), None)));
    }
    let targets = session.assistant_request_targets().unwrap_or_default();
    let devices = targets
        .iter()
        .map(|target| target.device_id.clone())
        .collect::<Vec<_>>();
    let Some(audience) = request_memory_target(&devices, draft.target_ref.as_deref()) else {
        return Ok(None);
    };
    let instance = session
        .assistant_data
        .as_ref()
        .and_then(|data| data.source_instance_id.as_deref())
        .ok_or(AssistantStoreError::Inaccessible)?;
    let reference = super::source_device_ref(instance, audience);
    let eligible = super::current_memory::fact_devices_on(db, session, scope).await?;
    Ok(eligible
        .contains(&reference)
        .then(|| (reference, Some(audience.to_owned()))))
}

async fn ensure_device_binding_on(
    db: &DatabaseTransaction,
    reference: &str,
    instance: &str,
    audience: &str,
    now: u64,
) -> Result<(), AssistantStoreError> {
    use crate::entity::agent_assistant_device_binding as binding;
    binding::Entity::insert(binding::ActiveModel {
        binding_id: Set(reference.into()),
        instance_id: Set(instance.into()),
        audience: Set(audience.into()),
        first_seen_at_ms: Set(signed(now)?),
        last_seen_at_ms: Set(signed(now)?),
        ..Default::default()
    })
    .on_conflict(
        OnConflict::columns([binding::Column::InstanceId, binding::Column::Audience])
            .do_nothing()
            .to_owned(),
    )
    .exec_without_returning(db)
    .await?;
    let bound = binding::Entity::find()
        .filter(binding::Column::BindingId.eq(reference))
        .filter(binding::Column::InstanceId.eq(instance))
        .filter(binding::Column::Audience.eq(audience))
        .one(db)
        .await?;
    if bound.is_none() {
        return Err(AssistantStoreError::Inaccessible);
    }
    Ok(())
}

fn skipped(reason: &str) -> MemoryWriteResult {
    let outcome = match reason {
        "stale_target" => MemoryWriteOutcome::Conflict,
        "content_retention_denied"
        | "content_expired"
        | "target_not_observed"
        | "source_not_observed"
        | "source_out_of_scope" => MemoryWriteOutcome::Rejected,
        _ => MemoryWriteOutcome::Skipped,
    };
    shared::result(outcome, reason, None)
}

/// The source id of an owner input: its first history fragment.
async fn input_source_id_on(
    db: &DatabaseTransaction,
    actor: &str,
    origin_id: &str,
) -> Result<Option<String>, AssistantStoreError> {
    Ok(history::Entity::find()
        .filter(history::Column::ActorId.eq(actor))
        .filter(history::Column::OriginId.eq(origin_id))
        .filter(history::Column::FragmentIndex.eq(0))
        .one(db)
        .await?
        .map(|row| row.source_id()))
}
