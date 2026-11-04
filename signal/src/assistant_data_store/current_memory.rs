//! Bounded current-fact selection under original native preparation fences.
//! All original source/session reads precede the final read-proof quota write.

use super::{
    AssistantStoreError,
    conversation_search::now_ms,
    memory_view::{self, AuthorizedMemoryView, MemoryReadAuthorization},
    signed, unsigned,
};
use crate::config::connection::{DatabaseConnection, DatabaseTransaction};
use crate::entity::{agent_assistant_read_projection as projection, agent_memory as memory};
use desk_agent_protocol::{
    ai_assistant::{memory::MemoryUsePurpose, source::SourceReadPurpose},
    data_lineage::DestinationIdentity,
};
use desk_diagnose_core::{
    ai_assistant::{
        current_memory::CurrentMemoryPayload,
        data_access::scope_sha256,
        memory_access::{MemoryAccessScope, usable_memory_scope},
        read_projection::{CurrentMemoryPreparation, ReadProjectionKind, ReadProjectionProof},
        retrieval_query::QuerySourceVersion,
        seam::{AssistantDataReadUse, CurrentMemoryBlock},
    },
    session::{AgentSessionSurface, PersistedAgentSession},
};
use sea_orm::{
    ColumnTrait, ConnectionTrait, EntityTrait, FromQueryResult, QueryFilter, QueryOrder,
    QuerySelect, TransactionTrait, sea_query::Condition,
};
use std::{
    collections::BTreeSet,
    time::{Duration, Instant},
};

pub(super) async fn fact_devices_on<C: ConnectionTrait>(
    db: &C,
    session: &PersistedAgentSession,
    scope: &MemoryAccessScope,
) -> Result<BTreeSet<String>, AssistantStoreError> {
    if scope.request_device_refs.is_empty() {
        return Ok(scope.current_device_refs().iter().cloned().collect());
    }
    if !session.has_admitted_personal_input() {
        return Err(AssistantStoreError::Inaccessible);
    }
    let ids =
        desk_assistant_data_store::device_roster::current_request_devices(db, session).await?;
    let instance = session
        .assistant_data
        .as_ref()
        .and_then(|data| data.source_instance_id.as_deref())
        .ok_or(AssistantStoreError::Inaccessible)?;
    let devices = crate::agent_run_event_store::eligible_request_device_ids(db, &ids)
        .await
        .map_err(|_| AssistantStoreError::Inaccessible)?;
    Ok(devices
        .iter()
        .map(|id| super::source_device_ref(instance, id))
        .filter(|reference| scope.request_device_refs.contains(reference))
        .collect())
}

pub(crate) async fn prepare(
    db: &DatabaseConnection,
    session: &PersistedAgentSession,
    destination: &DestinationIdentity,
    preparation_id: &str,
    maximum_bytes: usize,
) -> Result<CurrentMemoryBlock, AssistantStoreError> {
    if session.surface != AgentSessionSurface::AiAssistant
        || !session.agent_role.is_main()
        || maximum_bytes == 0
        || !matches!(destination, DestinationIdentity::Model { .. })
        || destination.validate().is_err()
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    let data = session
        .assistant_data
        .as_ref()
        .ok_or(AssistantStoreError::Inaccessible)?;
    ReadProjectionProof::current_logical_id(session, preparation_id)
        .map_err(AssistantStoreError::Invalid)?;
    if data.temporary || !desk_diagnose_core::ai_assistant::data_access::eligible_origin(session) {
        return Ok(empty(data.memory_state_generation));
    }
    let txn = db.begin().await?;
    match desk_diagnose_core::future::boxed(|| {
        prepare_on(&txn, session, destination, preparation_id, maximum_bytes)
    })
    .await
    {
        Ok(result) => {
            txn.commit().await?;
            Ok(result)
        }
        Err(error) => {
            txn.rollback().await?;
            Err(error)
        }
    }
}

async fn prepare_on(
    db: &DatabaseTransaction,
    session: &PersistedAgentSession,
    destination: &DestinationIdentity,
    preparation_id: &str,
    maximum_bytes: usize,
) -> Result<CurrentMemoryBlock, AssistantStoreError> {
    let now = now_ms()?;
    let fence =
        desk_diagnose_core::future::boxed(|| super::current_native::prepare_on(db, session, now))
            .await?;
    if !fence.enabled {
        fence.finish_on(now_ms()?.max(now)).await?;
        return Ok(empty(fence.access.memory_state_generation));
    }
    let scope = usable_memory_scope(session, now).ok_or(AssistantStoreError::Inaccessible)?;
    let scope_hash = scope_sha256(session).map_err(AssistantStoreError::Invalid)?;
    let devices = desk_diagnose_core::future::boxed(|| fact_devices_on(db, session, scope))
        .await?
        .into_iter()
        .take(fence.policy.limits.retrieval.episode.query_devices as usize)
        .collect();
    // A withdrawn device changes the allowed fact set even within the same
    // input and memory generation. Keep the original preparation immutable.
    let selected_preparation_id;
    let preparation_id = if scope.request_device_refs.is_empty() {
        preparation_id
    } else {
        use desk_diagnose_core::native_ids::{self, Kind};
        selected_preparation_id = native_ids::stable(
            Kind::AssistantReadProjection,
            &("selected-memory-devices", preparation_id, &devices),
        );
        &selected_preparation_id
    };
    let logical = ReadProjectionProof::current_logical_id(session, preparation_id)
        .map_err(AssistantStoreError::Invalid)?;
    let episode = &session
        .assistant_data
        .as_ref()
        .ok_or(AssistantStoreError::Inaccessible)?
        .retrieval_budget
        .episode_id;
    let existing = projection::Entity::find()
        .filter(projection::Column::ActorId.eq(&session.actor_id))
        .filter(projection::Column::RootConversationId.eq(&session.conversation_id))
        .filter(projection::Column::RetrievalEpisodeId.eq(episode))
        .filter(projection::Column::LogicalCallId.eq(&logical))
        .one(db)
        .await?;
    let authority = MemoryReadAuthorization {
        actor_id: &session.actor_id,
        current_device_ref: &scope.current_device_ref,
        scope_sha256: &scope_hash,
        scope,
        access: &fence.access,
        platform: &fence.policy,
        destination,
        requested_devices: &devices,
        observation_generation: fence.access.memory_state_generation,
        now_unix_ms: now,
    };
    let context = CurrentMemoryPreparation {
        session,
        access: &fence.access,
        destination,
        preparation_id,
        maximum_bytes,
        authority_deadline: fence.deadline,
        now,
    };
    if let Some(row) = existing {
        let stored = super::projection::decode_row(row)?;
        if stored.row.revoked_at_ms.is_some()
            || stored.proof.kind != ReadProjectionKind::CurrentMemories
        {
            return Err(AssistantStoreError::Inaccessible);
        }
        let content = stored
            .row
            .result_json
            .as_deref()
            .ok_or(AssistantStoreError::Inaccessible)?;
        let consumed = stored.row.first_consumed_at_ms.map(unsigned).transpose()?;
        stored
            .proof
            .validate_current_replay(context, content, consumed)
            .map_err(AssistantStoreError::Invalid)?;
        let payload = CurrentMemoryPayload::decode(content, maximum_bytes, now)
            .map_err(AssistantStoreError::Invalid)?;
        desk_diagnose_core::future::boxed(|| {
            validate_payload_on(db, &authority, &payload, &stored.proof.usage)
        })
        .await?;
        let deadline = fence.finish_on(now_ms()?.max(now)).await?;
        live(&stored.proof, consumed, deadline, now_ms()?.max(now))?;
        return Ok(block(payload, &stored.proof));
    }
    let views = desk_diagnose_core::future::boxed(|| select_on(db, &authority, context)).await?;
    if views.is_empty() {
        fence.finish_on(now_ms()?.max(now)).await?;
        return Ok(empty(fence.access.memory_state_generation));
    }
    let payload = CurrentMemoryPayload::new(
        fence.access.memory_state_generation,
        views.iter().map(|view| view.record.clone()).collect(),
    );
    let content = payload
        .encode(maximum_bytes, now)
        .map_err(AssistantStoreError::Invalid)?
        .ok_or(AssistantStoreError::Capacity)?;
    let selected = views
        .iter()
        .map(|view| view.usage.clone())
        .collect::<Vec<_>>();
    let mut proof = ReadProjectionProof::for_current_memories(context, &content, &selected)
        .map_err(AssistantStoreError::Invalid)?;
    let fresh_at = now_ms()?.max(now);
    let view = desk_diagnose_core::ai_assistant::retained_copy::OwnerHistoryReadValidation::for_prepared_projection(&proof)
        .map_err(AssistantStoreError::Invalid)?;
    proof.snapshot_policy = super::owner_copy::inspect_on(db, &view, now)
        .await?
        .map(|copy| copy.policy);
    let fresh_authority = MemoryReadAuthorization {
        now_unix_ms: fresh_at,
        ..authority
    };
    desk_diagnose_core::future::boxed(|| {
        validate_payload_on(db, &fresh_authority, &payload, &proof.usage)
    })
    .await?;
    let deadline = fence.finish_on(now_ms()?.max(now)).await?;
    live(&proof, None, deadline, now_ms()?.max(now))?;
    // No original source or native SELECT/UPDATE may follow this quota phase.
    let stored =
        super::projection::prepare_on(db, &proof, &content, &fence.policy, now_ms()?.max(now))
            .await?;
    if stored.proof.as_ref() != &proof {
        return Err(AssistantStoreError::Inaccessible);
    }
    live(&stored.proof, None, deadline, now_ms()?.max(now))?;
    Ok(block(payload, &stored.proof))
}

#[derive(FromQueryResult)]
struct Candidate {
    memory_id: String,
    revision: i64,
    content_sha256: String,
    updated_at_ms: i64,
}

async fn select_on(
    db: &DatabaseTransaction,
    context: &MemoryReadAuthorization<'_>,
    preparation: CurrentMemoryPreparation<'_>,
) -> Result<Vec<Box<AuthorizedMemoryView>>, AssistantStoreError> {
    let mut selected: Vec<Box<AuthorizedMemoryView>> = Vec::new();
    let mut records = Vec::new();
    let mut cursor: Option<(i64, String)> = None;
    let began = Instant::now();
    let mut visited = 0_u64;
    let maximum = u64::from(context.platform.max_retrieval_document_visits)
        .min(context.platform.max_memory_records);
    loop {
        if selected.len() >= context.platform.limits.memory.current_facts as usize
            || visited >= maximum
            || began.elapsed()
                >= Duration::from_millis(
                    context
                        .platform
                        .limits
                        .retrieval
                        .current_memory_selection_ms,
                )
        {
            break;
        }
        let limit = u64::from(context.platform.limits.processing.current_memory_page_size)
            .min(maximum - visited);
        let mut query = memory::Entity::find()
            .select_only()
            .columns([
                memory::Column::MemoryId,
                memory::Column::Revision,
                memory::Column::ContentSha256,
                memory::Column::UpdatedAtMs,
            ])
            .filter(memory::Column::ActorId.eq(context.actor_id))
            .filter(memory::Column::Status.eq("active"))
            .filter(memory::Column::DeviceRef.is_in(
                std::iter::once(String::new()).chain(context.requested_devices.iter().cloned()),
            ))
            .filter(memory::Column::StateGeneration.lte(signed(context.observation_generation)?))
            .filter(memory::Column::ValidFromMs.gt(0))
            .filter(memory::Column::ValidFromMs.lte(signed(context.now_unix_ms)?))
            .filter(
                Condition::any()
                    .add(memory::Column::ValidUntilMs.is_null())
                    .add(memory::Column::ValidUntilMs.gt(signed(context.now_unix_ms)?)),
            )
            .order_by_desc(memory::Column::UpdatedAtMs)
            .order_by_asc(memory::Column::MemoryId)
            .limit(limit);
        if let Some((time, id)) = &cursor {
            query = query.filter(
                Condition::any()
                    .add(memory::Column::UpdatedAtMs.lt(*time))
                    .add(
                        Condition::all()
                            .add(memory::Column::UpdatedAtMs.eq(*time))
                            .add(memory::Column::MemoryId.gt(id)),
                    ),
            );
        }
        let rows = query.into_model::<Candidate>().all(db).await?;
        let finished = rows.len() < limit as usize;
        for candidate in rows {
            if began.elapsed()
                >= Duration::from_millis(
                    context
                        .platform
                        .limits
                        .retrieval
                        .current_memory_selection_ms,
                )
                || selected.len() >= context.platform.limits.memory.current_facts as usize
            {
                break;
            }
            visited += 1;
            cursor = Some((candidate.updated_at_ms, candidate.memory_id.clone()));
            let version = QuerySourceVersion {
                source_id: candidate.memory_id,
                revision: unsigned(candidate.revision)?,
                content_sha256: candidate.content_sha256,
            };
            let result = desk_diagnose_core::future::boxed(|| {
                memory_view::read_on(db, context, &version, MemoryUsePurpose::CurrentFact)
            })
            .await;
            let view = match result {
                Ok(view) => view,
                Err(AssistantStoreError::Backend(error)) => return Err(error.into()),
                Err(_) => {
                    memory_view::validate_context_on(db, context).await?;
                    continue;
                }
            };
            if began.elapsed()
                >= Duration::from_millis(
                    context
                        .platform
                        .limits
                        .retrieval
                        .current_memory_selection_ms,
                )
            {
                break;
            }
            records.push(view.record.clone());
            let content =
                CurrentMemoryPayload::new(context.access.memory_state_generation, records.clone())
                    .encode(preparation.maximum_bytes, context.now_unix_ms)
                    .ok()
                    .flatten();
            let fits = if let Some(content) = content {
                let mut usages = selected
                    .iter()
                    .map(|view| view.usage.clone())
                    .collect::<Vec<_>>();
                usages.push(view.usage.clone());
                // Optional automatic recall also observes the full metadata
                // bound. An oversized/conflicting dependency union is omitted;
                // it is never truncated or given an incomplete source label.
                ReadProjectionProof::for_current_memories(preparation, &content, &usages).is_ok()
            } else {
                false
            };
            if fits {
                selected.push(view);
            } else {
                records.pop();
            }
        }
        if finished {
            break;
        }
    }
    Ok(selected)
}

async fn validate_payload_on(
    db: &DatabaseTransaction,
    context: &MemoryReadAuthorization<'_>,
    payload: &CurrentMemoryPayload,
    usage: &AssistantDataReadUse,
) -> Result<(), AssistantStoreError> {
    for record in &payload.memories {
        let version = QuerySourceVersion {
            source_id: record.memory_id.clone(),
            revision: record.revision,
            content_sha256: super::memory::content_hash(&record.content, record.context.as_deref()),
        };
        let current = desk_diagnose_core::future::boxed(|| {
            memory_view::read_on(db, context, &version, MemoryUsePurpose::CurrentFact)
        })
        .await?;
        if current.record != *record {
            return Err(AssistantStoreError::Inaccessible);
        }
    }
    let expiry = super::model_sources::validate_memory_devices_on(
        db,
        context.actor_id,
        context.requested_devices,
        usage,
        context.access,
        context.destination,
        context.platform,
        context.now_unix_ms,
    )
    .await?;
    if expiry != usage.expires_at_unix_ms || usage.purpose != SourceReadPurpose::ModelMemory {
        return Err(AssistantStoreError::Inaccessible);
    }
    Ok(())
}

fn live(
    proof: &ReadProjectionProof,
    consumed: Option<u64>,
    deadline: u64,
    now: u64,
) -> Result<(), AssistantStoreError> {
    if now >= deadline || !proof.preparation_available(now, consumed) {
        return Err(AssistantStoreError::Inaccessible);
    }
    Ok(())
}
fn empty(generation: u64) -> CurrentMemoryBlock {
    CurrentMemoryBlock {
        memory_state_generation: generation,
        records: Vec::new(),
        source_envelopes: Vec::new(),
        uses: Vec::new(),
    }
}
fn block(payload: CurrentMemoryPayload, proof: &ReadProjectionProof) -> CurrentMemoryBlock {
    CurrentMemoryBlock {
        memory_state_generation: payload.memory_state_generation,
        records: payload.memories,
        source_envelopes: vec![proof.source_envelope.clone()],
        uses: vec![proof.usage.clone()],
    }
}
