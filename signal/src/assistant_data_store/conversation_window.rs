//! Exact observed anchors and bounded original windows. The query freezes source
//! versions and admission order; a missing anchor never selects another chat.

use super::{
    AssistantStoreError,
    conversation_search::{now_ms, search_authority, validate_final_time},
    history_scan::WindowPosition,
    history_view::AuthorizedHistoryView,
    keyword_search::HistorySearchAuthorization,
    retrieval_cache::{self, StoredQuery},
    retrieval_native::NativeAssistantDataFence,
    retrieval_result::{self, RetrievalResultInput},
    unsigned,
};
use crate::config::connection::{DatabaseConnection, DatabaseTransaction};
use desk_agent_protocol::ai_assistant::{
    history::{ConversationReadRequest, ConversationReadResult, ConversationReadView},
    source::{SourceAvailability, SourceReadPurpose},
};
use desk_diagnose_core::{
    ai_assistant::{
        retrieval_query::{PageAnchor, QuerySourceVersion},
        retrieval_response::{ToolReplyBudget, WindowLayout, WindowPageTokens, fit_window},
        retrieval_state::{
            FrozenRetrievalQuery, FrozenRetrievalRequest, QueryCursor, RetrievalQueryBinding,
            RetrievalSelection, RetrievalWatermark,
        },
        seam::{AssistantDataCall, AssistantDataRead, AssistantDataReadUse},
    },
    native_ids::{self, Kind},
};
use sea_orm::TransactionTrait;
use std::collections::BTreeSet;

pub(crate) async fn read(
    db: &DatabaseConnection,
    context: AssistantDataCall<'_>,
    request: &ConversationReadRequest,
) -> Result<AssistantDataRead<ConversationReadResult>, AssistantStoreError> {
    let txn = db.begin().await?;
    match desk_diagnose_core::future::boxed(|| read_on(&txn, context, request)).await {
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
    request: &ConversationReadRequest,
) -> Result<AssistantDataRead<ConversationReadResult>, AssistantStoreError> {
    let now = now_ms()?;
    let context = AssistantDataCall {
        now_unix_ms: now,
        ..context
    };
    let fence =
        desk_diagnose_core::future::boxed(|| super::retrieval_native::prepare_on(db, context, now))
            .await?;
    if let Some(result) = desk_diagnose_core::future::boxed(|| {
        retrieval_result::replay_on::<_, ConversationReadResult>(
            db,
            &fence.context,
            &fence.access,
            &fence.policy,
            SourceReadPurpose::ModelHistory,
            now,
        )
    })
    .await?
    {
        let deadline = fence.finish_on(now_ms()?).await?;
        validate_final_time(&result, deadline, now_ms()?)?;
        return Ok(result);
    }
    let binding = RetrievalQueryBinding::for_call(&fence.context, SourceReadPurpose::ModelHistory)
        .map_err(AssistantStoreError::Invalid)?;
    let old = if request.cursor.is_some() {
        Some(retrieval_cache::load_window_cursor_on(db, &binding, request, now).await?)
    } else {
        retrieval_cache::load_window_logical_on(db, &binding, context.logical_call_id, request, now)
            .await?
    };
    if old
        .as_ref()
        .is_some_and(|old| old.proof.was_delivered(context.logical_call_id))
    {
        return Err(AssistantStoreError::Invalid(
            "original delivered retrieval response is unavailable",
        ));
    }
    let prepared = desk_diagnose_core::future::boxed(|| {
        prepare_query_on(db, &fence, binding, request, old.as_ref())
    })
    .await?;
    let PreparedWindow {
        mut query,
        anchor: original_anchor,
    } = *prepared;
    let page = request
        .cursor
        .as_deref()
        .map(|raw| {
            query
                .page(raw, &query.binding, &query.request, now_ms()?)
                .cloned()
                .map_err(AssistantStoreError::Invalid)
        })
        .transpose()?;
    let authority = search_authority(&fence, &query, now_ms()?)?;
    let devices = BTreeSet::from([original_anchor.source.row.device_ref.clone()]);
    let scanned = desk_diagnose_core::future::boxed(|| {
        super::history_scan::window::read_on(db, &authority, &query, page.as_ref(), &devices)
    })
    .await?;
    let mut reasons = query.reasons.clone();
    reasons.extend(scanned.reasons);
    let mut views = Vec::new();
    let mut positions = Vec::new();
    for item in scanned.items {
        match desk_diagnose_core::future::boxed(|| {
            super::history_view::read_on(db, &authority, &item.source)
        })
        .await
        {
            Ok(view) if position(&view)? == item.position => {
                match view.availability {
                    SourceAvailability::Partial => reasons.push("index_partial".into()),
                    SourceAvailability::Redacted => reasons.push("source_redacted".into()),
                    _ => {}
                }
                positions.push(item.position);
                views.push(view);
            }
            Err(AssistantStoreError::Backend(error)) => return Err(error.into()),
            _ => {
                reasons.push("source_unavailable".into());
                super::keyword_search::validate_context_on(db, &authority).await?;
            }
        }
    }
    let native_status = desk_diagnose_core::future::boxed(|| {
        super::native_summary::read_on(
            db,
            &authority,
            &original_anchor.source.row.root_conversation_id,
            &original_anchor.device,
        )
    })
    .await?;
    let native_status = native_status.filter(|native| !native.summary.items.is_empty());
    if native_status
        .as_ref()
        .is_some_and(|native| native.summary.has_more)
    {
        reasons.push("native_status_partial".into());
    }
    let anchor_position = scanned.anchor_position;
    let layout = match page {
        Some(PageAnchor::Window { forward: true, .. }) => WindowLayout::Forward,
        Some(PageAnchor::Window { forward: false, .. }) => WindowLayout::Backward,
        None | Some(PageAnchor::Start) if request.view == ConversationReadView::Latest => {
            WindowLayout::Latest
        }
        None | Some(PageAnchor::Start) => WindowLayout::Context {
            focus_index: positions
                .iter()
                .position(|position| position >= &anchor_position)
                .unwrap_or(positions.len()),
        },
        _ => return Err(AssistantStoreError::Inaccessible),
    };
    // Index coverage and native related-result aggregation have independent
    // authority. A bounded text window alone cannot prove their completeness.
    reasons.push("coverage_not_exhaustive".into());
    reasons.sort();
    reasons.dedup();
    if reasons.len() > 31 {
        reasons.truncate(30);
        reasons.push("reason_limit".into());
        reasons.sort();
        reasons.dedup();
    }
    let previous = page_cursor(&query.snapshot.query_id, context.logical_call_id, false)?;
    let next = page_cursor(&query.snapshot.query_id, context.logical_call_id, true)?;
    let result = ConversationReadResult {
        conversation_id: original_anchor.source.row.conversation_id.clone(),
        messages: views
            .iter()
            .map(|view| view.message())
            .collect::<Result<_, _>>()?,
        related_results: native_status
            .as_ref()
            .map(|native| native.summary.items.clone())
            .unwrap_or_default(),
        previous_cursor: scanned.previous.as_ref().map(|_| previous.1.clone()),
        next_cursor: scanned.next.as_ref().map(|_| next.1.clone()),
        source_observed_through_unix_ms: query.source_observed_at_unix_ms,
        actual_bytes: 0,
        has_more: scanned.has_more,
        partial: true,
        reasons,
    };
    let budget = ToolReplyBudget::new(
        &context.tool_call.id,
        context.maximum_response_bytes.min(request.max_bytes),
    )
    .map_err(AssistantStoreError::Invalid)?;
    let mut reply = fit_window(
        result,
        &budget,
        layout,
        WindowPageTokens {
            previous: &previous.1,
            next: &next.1,
        },
    )
    .map_err(AssistantStoreError::Invalid)?;
    if reply.result.previous_cursor.is_some() {
        let frontier = if reply.selected.start > 0 {
            positions
                .get(reply.selected.start)
                .map(|position| position.anchor(false))
        } else {
            scanned.previous
        };
        reply.result.previous_cursor = Some(
            query
                .add_page(
                    previous.0,
                    frontier.ok_or(AssistantStoreError::Inaccessible)?,
                )
                .map_err(AssistantStoreError::Invalid)?,
        );
    }
    if reply.result.next_cursor.is_some() {
        let frontier = if reply.selected.end < views.len() {
            reply
                .selected
                .end
                .checked_sub(1)
                .and_then(|index| positions.get(index))
                .map(|position| position.anchor(true))
        } else {
            scanned.next
        };
        reply.result.next_cursor = Some(
            query
                .add_page(next.0, frontier.ok_or(AssistantStoreError::Inaccessible)?)
                .map_err(AssistantStoreError::Invalid)?,
        );
    }
    // The actual canonical tokens participate in the self-inclusive byte count.
    budget
        .try_encode_window(&mut reply.result)
        .map_err(AssistantStoreError::Invalid)?
        .ok_or(AssistantStoreError::Capacity)?;
    let mut versions = views[reply.selected.clone()]
        .iter()
        .map(|view| view.version())
        .collect::<Result<Vec<_>, _>>()?;
    // Re-read even when latest omitted the old anchor from the returned tail.
    // The locator never grants a fallback to a newer version or another chat.
    let (anchor_version, conversation) = window_anchor(&query)?;
    let current = search_authority(&fence, &query, now_ms()?)?;
    let anchor = desk_diagnose_core::future::boxed(|| {
        super::history_view::read_on(db, &current, anchor_version)
    })
    .await?;
    if anchor.source.row.conversation_id != conversation || position(&anchor)? != anchor_position {
        return Err(AssistantStoreError::Inaccessible);
    }
    let mut selected = views[reply.selected]
        .iter()
        .map(|view| view.source.usage.clone())
        .collect::<Vec<_>>();
    selected.push(decision_use(&anchor.source.usage));
    if let Some(native) = &native_status {
        // The window only publishes its three related states. Observations
        // used solely for aggregate counts are not delivered or remembered.
        for source in native.sources().filter(|source| {
            reply
                .result
                .related_results
                .iter()
                .any(|item| item.source_id == source.row.source_id)
        }) {
            if !versions
                .iter()
                .any(|version| version.source_id == source.row.source_id)
            {
                versions.push(source.version()?);
                selected.push(source.usage.clone());
            }
        }
    }

    query
        .remember(&versions)
        .map_err(AssistantStoreError::Invalid)?;
    let prepared_at = now_ms()?;
    let pending = desk_diagnose_core::future::boxed(|| {
        retrieval_result::prepare_on(
            db,
            &fence.context,
            &fence.access,
            &fence.policy,
            RetrievalResultInput {
                result: reply.result,
                purpose: SourceReadPurpose::ModelHistory,
                selected: &selected,
            },
            prepared_at,
        )
    })
    .await?;
    let deadline = fence.finish_on(now_ms()?).await?;
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
    let saved = if let Some(old) = &old {
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

struct PreparedWindow {
    query: Box<FrozenRetrievalQuery>,
    anchor: Box<AuthorizedHistoryView>,
}

async fn prepare_query_on(
    db: &DatabaseTransaction,
    fence: &NativeAssistantDataFence<'_>,
    binding: RetrievalQueryBinding,
    request: &ConversationReadRequest,
    old: Option<&StoredQuery>,
) -> Result<Box<PreparedWindow>, AssistantStoreError> {
    let scope = binding
        .history_scope
        .as_ref()
        .ok_or(AssistantStoreError::Inaccessible)?;
    let runtime = super::runtime_on(db, &binding.actor_id).await?;
    let observed_at = now_ms()?;
    let watermark = RetrievalWatermark {
        corpus_generation: unsigned(runtime.history_data_generation)?,
        memory_generation: fence.access.memory_state_generation,
        control_generation: fence.access.control_generation,
        policy_revision: fence.policy.revision,
    };
    let version = if let Some(old) = old {
        window_anchor(&old.proof)?.0.clone()
    } else {
        fence
            .observed_version(&request.source_id)
            .ok_or(AssistantStoreError::Inaccessible)?
            .clone()
    };
    if request.cursor.is_none() && fence.observed_version(&request.source_id) != Some(&version) {
        return Err(AssistantStoreError::Inaccessible);
    }
    let authority = HistorySearchAuthorization {
        actor_id: &binding.actor_id,
        current_device_ref: &binding.source_device_ref,
        scope_sha256: &binding.scope_sha256,
        scope,
        access: &fence.access,
        platform: &fence.policy,
        destination: fence.context.destination,
        corpus_generation: old.map_or(watermark.corpus_generation, |old| {
            old.proof.source_observation_generation
        }),
        now_unix_ms: observed_at,
    };
    let anchor = desk_diagnose_core::future::boxed(|| {
        super::history_view::read_on(db, &authority, &version)
    })
    .await?;
    let devices = vec![anchor.source.row.device_ref.clone()];
    let query = if let Some(old) = old {
        if window_anchor(&old.proof)?.1 != anchor.source.row.conversation_id {
            return Err(AssistantStoreError::Inaccessible);
        }
        let mut query = old.proof.clone();
        if query
            .needs_refresh(watermark)
            .map_err(AssistantStoreError::Invalid)?
            || query.snapshot.device_refs != devices
        {
            query
                .refresh(
                    RetrievalSelection {
                        devices,
                        watermark,
                        candidates: Vec::new(),
                        reasons: Vec::new(),
                    },
                    observed_at,
                )
                .map_err(AssistantStoreError::Invalid)?;
        }
        query
    } else {
        Box::new(
            FrozenRetrievalQuery::prepare(
                binding,
                fence
                    .context
                    .retrieval_reservation
                    .ok_or(AssistantStoreError::Inaccessible)?,
                FrozenRetrievalRequest::Window {
                    request: request.clone(),
                    anchor: version,
                    conversation_id: anchor.source.row.conversation_id.clone(),
                },
                RetrievalSelection {
                    devices,
                    watermark,
                    candidates: Vec::new(),
                    reasons: Vec::new(),
                },
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
        )
    };
    Ok(Box::new(PreparedWindow { query, anchor }))
}

fn window_anchor(
    query: &FrozenRetrievalQuery,
) -> Result<(&QuerySourceVersion, &str), AssistantStoreError> {
    if let FrozenRetrievalRequest::Window {
        anchor,
        conversation_id,
        ..
    } = &query.request
    {
        Ok((anchor, conversation_id))
    } else {
        Err(AssistantStoreError::Inaccessible)
    }
}
fn position(view: &AuthorizedHistoryView) -> Result<WindowPosition, AssistantStoreError> {
    Ok(WindowPosition {
        admitted_generation: unsigned(view.source.row.admitted_generation)?,
        fragment_index: u32::try_from(view.source.row.fragment_index)
            .map_err(|_| AssistantStoreError::Inaccessible)?,
        source_id: view.source.row.source_id.clone(),
    })
}
pub(super) fn page_cursor(
    query_id: &str,
    logical_call_id: &str,
    forward: bool,
) -> Result<(String, String), AssistantStoreError> {
    let page = native_ids::stable(
        Kind::AssistantPageCursor,
        &("window-page-v1", query_id, logical_call_id, forward),
    );
    let cursor = QueryCursor::new(query_id, &page)
        .map_err(AssistantStoreError::Invalid)?
        .encode();
    Ok((page, cursor))
}

fn decision_use(anchor: &AssistantDataReadUse) -> AssistantDataReadUse {
    let mut decision = anchor.clone();
    decision.decision_roots = std::mem::take(&mut decision.roots);
    decision
}
