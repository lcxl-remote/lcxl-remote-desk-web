//! Actual original model-tool admission and atomic memory search delivery.
//! This is separate from owner management and does not create write authority.

use super::{
    AssistantStoreError,
    conversation_search::{now_ms, page_cursor, validate_final_time},
    memory_view::{self, AuthorizedMemoryView, MemoryReadAuthorization},
    retrieval_cache::{self, StoredQuery},
    retrieval_native::NativeAssistantDataFence,
    retrieval_result::{self, RetrievalResultInput},
    unsigned,
};
use crate::config::connection::{DatabaseConnection, DatabaseTransaction};
use desk_agent_protocol::ai_assistant::{
    history::{AssistantDeviceScope, AssistantHistoryCoverage},
    memory::{MemorySearchRequest, MemorySearchResult, MemoryUsePurpose},
    source::SourceReadPurpose,
};
use desk_diagnose_core::ai_assistant::{
    memory_access::usable_memory_scope,
    retrieval::query_groups,
    retrieval_query::{PageAnchor, QuerySourceVersion},
    retrieval_response::{ToolReplyBudget, fit_memories},
    retrieval_state::{
        FrozenRetrievalQuery, FrozenRetrievalRequest, RetrievalQueryBinding, RetrievalSelection,
        RetrievalWatermark,
    },
    seam::{AssistantDataCall, AssistantDataRead},
};
use sea_orm::TransactionTrait;
use std::collections::BTreeSet;

pub(crate) async fn search(
    db: &DatabaseConnection,
    context: AssistantDataCall<'_>,
    request: &MemorySearchRequest,
) -> Result<AssistantDataRead<MemorySearchResult>, AssistantStoreError> {
    let txn = db.begin().await?;
    match desk_diagnose_core::future::boxed(|| search_on(&txn, context, request)).await {
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

async fn search_on(
    db: &DatabaseTransaction,
    context: AssistantDataCall<'_>,
    request: &MemorySearchRequest,
) -> Result<AssistantDataRead<MemorySearchResult>, AssistantStoreError> {
    let now = now_ms()?;
    let context = AssistantDataCall {
        now_unix_ms: now,
        ..context
    };
    let fence =
        desk_diagnose_core::future::boxed(|| super::retrieval_native::prepare_on(db, context, now))
            .await?;
    if let Some(result) = desk_diagnose_core::future::boxed(|| {
        retrieval_result::replay_on::<_, MemorySearchResult>(
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
    let original = FrozenRetrievalRequest::Memories {
        request: request.clone(),
    };
    let stored = if let Some(cursor) = &request.cursor {
        Some(retrieval_cache::load_cursor_on(db, &binding, &original, cursor, now).await?)
    } else {
        retrieval_cache::load_logical_on(db, &binding, context.logical_call_id, &original, now)
            .await?
    };
    if stored
        .as_ref()
        .is_some_and(|stored| stored.proof.was_delivered(context.logical_call_id))
    {
        return Err(AssistantStoreError::Invalid(
            "original delivered memory response is unavailable",
        ));
    }
    let devices = memory_view::retrieval_devices_on(db, &fence, now).await?;
    let mut query = desk_diagnose_core::future::boxed(|| {
        prepare_query_on(
            db,
            &fence,
            QueryPreparation {
                request,
                binding,
                original,
                old: stored.as_ref(),
                devices: &devices,
                now,
            },
        )
    })
    .await?;
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
    let query_devices = query.snapshot.device_refs.iter().cloned().collect();
    let authority = read_authority(
        &fence,
        &query.binding.scope,
        &query_devices,
        query.source_observation_generation,
        now,
    )?;
    let batch = desk_diagnose_core::future::boxed(|| {
        scan_on(db, &authority, &query, request, page.as_ref())
    })
    .await?;
    let mut effective_queries =
        query_groups(&request.queries).map_err(AssistantStoreError::Invalid)?;
    effective_queries.sort();
    let result = MemorySearchResult {
        memories: batch.views.iter().map(|view| view.record.clone()).collect(),
        effective_queries,
        has_more: batch.more,
        next_cursor: None,
        memory_state_generation: fence.access.memory_state_generation,
        coverage: coverage(batch.reasons),
    };
    let (page_id, provisional) = page_cursor(&query.snapshot.query_id, context.logical_call_id)?;
    let budget = ToolReplyBudget::new(&context.tool_call.id, context.maximum_response_bytes)
        .map_err(AssistantStoreError::Invalid)?;
    let mut reply =
        fit_memories(result, &budget, &provisional).map_err(AssistantStoreError::Invalid)?;
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
                .add_page(page_id, anchor.ok_or(AssistantStoreError::Inaccessible)?)
                .map_err(AssistantStoreError::Invalid)?,
        );
    }
    budget
        .encode(&reply.result)
        .map_err(AssistantStoreError::Invalid)?;
    let versions = batch.views[..reply.selected_count]
        .iter()
        .map(|view| view.version())
        .collect::<Result<Vec<_>, _>>()?;
    query
        .remember(&versions)
        .map_err(AssistantStoreError::Invalid)?;
    let selected = batch.views[..reply.selected_count]
        .iter()
        .map(|view| view.usage.clone())
        .collect::<Vec<_>>();
    let prepared_at = now_ms()?;
    let pending = desk_diagnose_core::future::boxed(|| {
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
    // All original native and body SELECTs precede the quota/result phase.
    let stored_at = now_ms()?;
    let result = desk_diagnose_core::future::boxed(|| {
        retrieval_result::store_on(pending, &fence.policy, stored_at)
    })
    .await?;
    if !query
        .mark_delivered(context.logical_call_id)
        .map_err(AssistantStoreError::Invalid)?
    {
        return Err(AssistantStoreError::Inaccessible);
    }
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

struct QueryPreparation<'a> {
    request: &'a MemorySearchRequest,
    binding: RetrievalQueryBinding,
    original: FrozenRetrievalRequest,
    old: Option<&'a StoredQuery>,
    devices: &'a BTreeSet<String>,
    now: u64,
}

async fn prepare_query_on(
    db: &DatabaseTransaction,
    fence: &NativeAssistantDataFence<'_>,
    input: QueryPreparation<'_>,
) -> Result<Box<FrozenRetrievalQuery>, AssistantStoreError> {
    let QueryPreparation {
        request,
        binding,
        original,
        old,
        devices,
        now,
    } = input;
    let scope =
        usable_memory_scope(fence.context.session, now).ok_or(AssistantStoreError::Inaccessible)?;
    if binding.memory_device_rule != Some(scope.device_rule)
        || (request.scope == AssistantDeviceScope::AllMyDevices
            && scope.device_rule != AssistantDeviceScope::AllMyDevices)
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    let runtime = super::runtime_on(db, &binding.actor_id).await?;
    let watermark = RetrievalWatermark {
        corpus_generation: unsigned(runtime.memory_state_generation)?,
        memory_generation: fence.access.memory_state_generation,
        control_generation: fence.access.control_generation,
        policy_revision: fence.policy.revision,
    };
    let allowed = devices
        .iter()
        .filter(|device| {
            request.scope == AssistantDeviceScope::AllMyDevices
                || scope.current_device_refs().contains(device)
        })
        .filter(|device| old.is_none_or(|old| old.proof.snapshot.device_refs.contains(device)))
        .cloned()
        .collect::<Vec<_>>();
    let changed = if let Some(old) = old {
        old.proof
            .needs_refresh(watermark)
            .map_err(AssistantStoreError::Invalid)?
            || old.proof.snapshot.device_refs != allowed
    } else {
        true
    };
    if !changed {
        return Ok(old.ok_or(AssistantStoreError::Inaccessible)?.proof.clone());
    }
    let observation = old.map_or(watermark.corpus_generation, |old| {
        old.proof.source_observation_generation
    });
    let allowed_set = allowed.iter().cloned().collect();
    let authority = read_authority(fence, &binding.scope, &allowed_set, observation, now)?;
    let candidates = desk_diagnose_core::future::boxed(|| {
        super::memory_keyword::relevance_on(db, &authority, request)
    })
    .await?;
    let selection = RetrievalSelection {
        devices: allowed,
        watermark,
        candidates: candidates.versions,
        reasons: candidates.reasons,
    };
    let observed_at = now_ms()?;
    if let Some(old) = old {
        let mut query = old.proof.clone();
        query
            .refresh(selection, observed_at)
            .map_err(AssistantStoreError::Invalid)?;
        Ok(query)
    } else {
        Ok(Box::new(
            FrozenRetrievalQuery::prepare(
                binding,
                fence
                    .context
                    .retrieval_reservation
                    .ok_or(AssistantStoreError::Inaccessible)?,
                original,
                selection,
                observed_at,
                fence
                    .context
                    .session
                    .assistant_data
                    .as_ref()
                    .unwrap()
                    .retrieval_budget
                    .limits,
            )
            .map_err(AssistantStoreError::Invalid)?,
        ))
    }
}

fn read_authority<'a>(
    fence: &'a NativeAssistantDataFence<'_>,
    assistant_scope: &'a desk_diagnose_core::ai_assistant::data_access::AssistantScope,
    devices: &'a BTreeSet<String>,
    observation: u64,
    now: u64,
) -> Result<MemoryReadAuthorization<'a>, AssistantStoreError> {
    let scope =
        usable_memory_scope(fence.context.session, now).ok_or(AssistantStoreError::Inaccessible)?;
    Ok(MemoryReadAuthorization {
        actor_id: &fence.context.session.actor_id,
        current_device_ref: &scope.current_device_ref,
        assistant_scope,
        scope,
        access: &fence.access,
        platform: &fence.policy,
        destination: fence.context.destination,
        requested_devices: devices,
        observation_generation: observation,
        now_unix_ms: now,
    })
}

#[allow(clippy::vec_box)]
struct SearchBatch {
    views: Vec<Box<AuthorizedMemoryView>>,
    frontiers: Vec<PageAnchor>,
    continuation: Option<PageAnchor>,
    more: bool,
    reasons: Vec<String>,
}

async fn scan_on(
    db: &DatabaseTransaction,
    context: &MemoryReadAuthorization<'_>,
    query: &FrozenRetrievalQuery,
    request: &MemorySearchRequest,
    page: Option<&PageAnchor>,
) -> Result<SearchBatch, AssistantStoreError> {
    let offset = match page {
        None | Some(PageAnchor::Start) => 0,
        Some(PageAnchor::Relevance { next_offset }) => *next_offset as usize,
        _ => return Err(AssistantStoreError::Inaccessible),
    };
    let mut cursor = offset;
    let mut candidates: Vec<(QuerySourceVersion, PageAnchor)> = Vec::new();
    while let Some(source) = query.snapshot.candidates.get(cursor) {
        if !query.was_seen(source) {
            candidates.push((
                source.clone(),
                PageAnchor::Relevance {
                    next_offset: (cursor + 1) as u32,
                },
            ));
        }
        cursor += 1;
        if candidates.len() >= request.limit as usize {
            break;
        }
    }
    let mut views = Vec::new();
    let mut frontiers = Vec::new();
    let mut reasons = query.reasons.clone();
    for (version, frontier) in candidates {
        match desk_diagnose_core::future::boxed(|| {
            memory_view::read_on(db, context, &version, MemoryUsePurpose::HistoricalReference)
        })
        .await
        {
            Ok(view) => {
                views.push(view);
                frontiers.push(frontier);
            }
            Err(AssistantStoreError::Backend(error)) => return Err(error.into()),
            Err(_) => {
                reasons.push("source_unavailable".into());
                memory_view::validate_context_on(db, context).await?;
            }
        }
    }
    let more = cursor < query.snapshot.candidates.len();
    Ok(SearchBatch {
        views,
        frontiers,
        more,
        continuation: more.then_some(PageAnchor::Relevance {
            next_offset: cursor as u32,
        }),
        reasons,
    })
}

fn coverage(mut reasons: Vec<String>) -> AssistantHistoryCoverage {
    // A complete extraction/job ledger is needed to claim that all eligible
    // statements have been maintained. Search hits alone cannot prove that.
    reasons.push("coverage_not_exhaustive".into());
    reasons.push("coverage_counts_lower_bounds".into());
    reasons.sort();
    reasons.dedup();
    AssistantHistoryCoverage {
        retained_from_unix_ms: None,
        retained_until_unix_ms: None,
        indexed_through_unix_ms: None,
        pending: 0,
        failed: 0,
        has_gaps: true,
        partial: true,
        reasons,
    }
}
