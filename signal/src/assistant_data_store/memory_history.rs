//! Read retained revisions through the original memory/tool transaction gates.

use super::{
    AssistantStoreError,
    conversation_search::{now_ms, page_cursor, validate_final_time},
    memory_view::{self, MemoryReadAuthorization},
    retrieval_cache,
    retrieval_native::NativeAssistantDataFence,
    retrieval_result::{self, RetrievalResultInput},
    unsigned,
};
use crate::config::connection::{DatabaseConnection, DatabaseTransaction};
use desk_agent_protocol::ai_assistant::{
    history::AssistantDeviceScope,
    memory::{MemoryHistoryRequest, MemoryHistoryResult},
    source::SourceReadPurpose,
};
use desk_diagnose_core::ai_assistant::{
    memory_access::usable_memory_scope,
    retrieval_response::{ToolReplyBudget, fit_memory_history},
    retrieval_state::{
        FrozenRetrievalQuery, FrozenRetrievalRequest, RetrievalQueryBinding, RetrievalSelection,
        RetrievalWatermark,
    },
    seam::{AssistantDataCall, AssistantDataRead},
};
use sea_orm::TransactionTrait;
use std::collections::BTreeSet;

pub(crate) async fn read(
    db: &DatabaseConnection,
    context: AssistantDataCall<'_>,
    request: &MemoryHistoryRequest,
) -> Result<AssistantDataRead<MemoryHistoryResult>, AssistantStoreError> {
    let txn = db.begin().await?;
    match desk_diagnose_core::future::send_phase(|| read_on(&txn, context, request)).await {
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

async fn read_on(
    db: &DatabaseTransaction,
    context: AssistantDataCall<'_>,
    request: &MemoryHistoryRequest,
) -> Result<AssistantDataRead<MemoryHistoryResult>, AssistantStoreError> {
    let now = now_ms()?;
    let context = AssistantDataCall {
        now_unix_ms: now,
        ..context
    };
    let fence = desk_diagnose_core::future::send_phase(|| {
        super::retrieval_native::prepare_on(db, context, now)
    })
    .await?;
    if let Some(result) = desk_diagnose_core::future::send_phase(|| {
        retrieval_result::replay_on::<_, MemoryHistoryResult>(
            db,
            &fence.context,
            &fence.access,
            &fence.policy,
            SourceReadPurpose::ModelMemory,
            now,
        )
    })
    .await?
    {
        let deadline = fence.finish_on(now_ms()?).await?;
        validate_final_time(&result, deadline, now_ms()?)?;
        return Ok(result);
    }
    let binding = RetrievalQueryBinding::for_call(&context, SourceReadPurpose::ModelMemory)
        .map_err(AssistantStoreError::Invalid)?;
    let devices = memory_view::retrieval_devices_on(db, &fence, now).await?;
    let scope =
        usable_memory_scope(context.session, now).ok_or(AssistantStoreError::Inaccessible)?;
    let mut devices = devices
        .into_iter()
        .filter(|device| {
            scope.device_rule == AssistantDeviceScope::AllMyDevices
                || scope.current_device_refs().contains(device)
        })
        .collect::<BTreeSet<_>>();
    let authority = read_authority(&fence, &binding.scope_sha256, &devices, now)?;
    memory_view::validate_context_on(db, &authority).await?;
    let head =
        desk_assistant_data_store::memory_history::head_on(db, &authority, &request.memory_id)
            .await?;
    let original = FrozenRetrievalRequest::MemoryHistory {
        request: request.clone(),
        current_revision: unsigned(head.revision)?,
    };
    let stored = if let Some(cursor) = &request.cursor {
        Some(retrieval_cache::load_cursor_on(db, &binding, &original, cursor, now).await?)
    } else {
        retrieval_cache::load_logical_on(db, &binding, context.logical_call_id, &original, now)
            .await?
    };
    let watermark = RetrievalWatermark {
        corpus_generation: fence.access.memory_state_generation,
        memory_generation: fence.access.memory_state_generation,
        control_generation: fence.access.control_generation,
        policy_revision: fence.access.policy_revision,
    };
    let mut query = if let Some(old) = &stored {
        if old.proof.was_delivered(context.logical_call_id) {
            return Err(AssistantStoreError::Invalid(
                "original delivered memory history response is unavailable",
            ));
        }
        devices.retain(|device| old.proof.snapshot.device_refs.contains(device));
        let mut query = old.proof.clone();
        if query
            .needs_refresh(watermark)
            .map_err(AssistantStoreError::Invalid)?
            || query
                .snapshot
                .device_refs
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>()
                != devices
        {
            query
                .refresh(
                    RetrievalSelection {
                        devices: devices.iter().cloned().collect(),
                        watermark,
                        candidates: Vec::new(),
                        reasons: Vec::new(),
                    },
                    now_ms()?,
                )
                .map_err(AssistantStoreError::Invalid)?;
        }
        query
    } else {
        Box::new(
            FrozenRetrievalQuery::prepare(
                binding,
                context
                    .retrieval_reservation
                    .ok_or(AssistantStoreError::Inaccessible)?,
                original,
                RetrievalSelection {
                    devices: devices.iter().cloned().collect(),
                    watermark,
                    candidates: Vec::new(),
                    reasons: Vec::new(),
                },
                now_ms()?,
                context
                    .session
                    .assistant_data
                    .as_ref()
                    .unwrap()
                    .retrieval_budget
                    .limits,
            )
            .map_err(AssistantStoreError::Invalid)?,
        )
    };
    let page = request
        .cursor
        .as_deref()
        .map(|cursor| {
            query
                .page(cursor, &query.binding, &query.request, now)
                .cloned()
        })
        .transpose()
        .map_err(AssistantStoreError::Invalid)?;
    let FrozenRetrievalRequest::MemoryHistory {
        current_revision, ..
    } = &query.request
    else {
        return Err(AssistantStoreError::Inaccessible);
    };
    let current_revision = *current_revision;
    let authority = read_authority(&fence, &query.binding.scope_sha256, &devices, now)?;
    let batch = desk_diagnose_core::future::send_phase(|| {
        desk_assistant_data_store::memory_history::scan_on::<_, memory_view::NativeMemoryReader>(
            db,
            &authority,
            &query,
            request,
            current_revision,
            page.as_ref(),
        )
    })
    .await?;
    let result = MemoryHistoryResult {
        memory_id: request.memory_id.clone(),
        current_revision,
        versions: batch.views.iter().map(|view| view.entry.clone()).collect(),
        has_more: batch.more,
        next_cursor: None,
        partial: !batch.reasons.is_empty(),
        reasons: batch.reasons,
    };
    let (page_id, token) = page_cursor(&query.snapshot.query_id, context.logical_call_id)?;
    let budget = ToolReplyBudget::new(&context.tool_call.id, context.maximum_response_bytes)
        .map_err(AssistantStoreError::Invalid)?;
    let mut reply =
        fit_memory_history(result, &budget, &token).map_err(|_| AssistantStoreError::Capacity)?;
    if reply.result.has_more {
        let anchor = if reply.selected_count < batch.views.len() {
            batch
                .frontiers
                .get(
                    reply
                        .selected_count
                        .checked_sub(1)
                        .ok_or(AssistantStoreError::Capacity)?,
                )
                .cloned()
        } else {
            batch.continuation
        };
        reply.result.next_cursor = Some(
            query
                .add_page(page_id, anchor.ok_or(AssistantStoreError::Capacity)?)
                .map_err(AssistantStoreError::Invalid)?,
        );
    }
    let versions = batch.views[..reply.selected_count]
        .iter()
        .map(|view| view.version.clone())
        .collect::<Vec<_>>();
    query
        .remember(&versions)
        .map_err(AssistantStoreError::Invalid)?;
    let selected = batch.views[..reply.selected_count]
        .iter()
        .map(|view| view.usage.clone())
        .collect::<Vec<_>>();
    let prepared_at = now_ms()?;
    let pending = desk_diagnose_core::future::send_phase(|| {
        retrieval_result::prepare_on(
            db,
            &fence.context,
            &fence.access,
            &fence.policy,
            RetrievalResultInput {
                result: reply.result,
                purpose: SourceReadPurpose::ModelMemory,
                selected: &selected,
            },
            prepared_at,
        )
    })
    .await?;
    let deadline = fence.finish_on(now_ms()?).await?;
    let stored_at = now_ms()?;
    let result = desk_diagnose_core::future::send_phase(|| {
        retrieval_result::store_on(pending, &fence.policy, stored_at)
    })
    .await?;
    query
        .mark_delivered(context.logical_call_id)
        .map_err(AssistantStoreError::Invalid)?;
    let saved = if let Some(old) = &stored {
        retrieval_cache::update_on(db, old, &query, &fence.policy, now_ms()?).await?
    } else {
        retrieval_cache::prepare_on(db, &query, &fence.policy, now_ms()?).await?
    };
    if saved.proof.as_ref() != query.as_ref() {
        return Err(AssistantStoreError::Inaccessible);
    }
    validate_final_time(
        &result,
        deadline.min(query.snapshot.expires_at_unix_ms),
        now_ms()?,
    )?;
    Ok(result)
}

fn read_authority<'a>(
    fence: &'a NativeAssistantDataFence<'_>,
    scope_hash: &'a str,
    devices: &'a BTreeSet<String>,
    now: u64,
) -> Result<MemoryReadAuthorization<'a>, AssistantStoreError> {
    let scope =
        usable_memory_scope(fence.context.session, now).ok_or(AssistantStoreError::Inaccessible)?;
    Ok(MemoryReadAuthorization {
        actor_id: &fence.context.session.actor_id,
        current_device_ref: &scope.current_device_ref,
        scope_sha256: scope_hash,
        scope,
        access: &fence.access,
        platform: &fence.policy,
        destination: fence.context.destination,
        requested_devices: devices,
        observation_generation: fence.access.memory_state_generation,
        now_unix_ms: now,
    })
}
