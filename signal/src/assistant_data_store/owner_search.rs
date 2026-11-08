//! Bounded owner search over the existing history index. It neither constructs
//! a model scope nor changes a retrieval budget or business execution state.
use super::{AssistantStoreError, signed, unsigned};
use crate::{
    config::ConfigConnection,
    entity::{
        agent_assistant_history as history, agent_assistant_runtime as runtime,
        agent_assistant_search_document as document, agent_assistant_search_term as term,
    },
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use desk_agent_protocol::ai_assistant::{
    history::HistorySort,
    history_window::AssistantHistoryFragment,
    owner_history::{AssistantHistorySearchRequest, AssistantHistorySearchResult},
    source::SourceAvailability,
    user_instructions::AssistantDataPolicy,
};
use desk_diagnose_core::ai_assistant::{
    retrieval::query_groups,
    retrieval_rank::{ReadableSearchDocument, rank_readable_documents},
};
use sea_orm::{
    ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect, QueryTrait,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::{Duration, Instant},
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    request_hash: String,
    watermark: u64,
    policy: u64,
    // Exact candidates, frozen before paging. A cursor grants no access: every
    // row, owner, scope, source revision and retention policy is checked again.
    remaining: Vec<(String, i64)>,
    reasons: Vec<String>,
}
impl Cursor {
    fn encode(&self, maximum: u32) -> Result<String, AssistantStoreError> {
        let raw = serde_json::to_vec(self).map_err(|_| AssistantStoreError::Inaccessible)?;
        if raw.len() > maximum as usize {
            return Err(AssistantStoreError::Capacity);
        }
        Ok(format!("oh1_{}", URL_SAFE_NO_PAD.encode(raw)))
    }
}
fn matches(
    row: &history::Model,
    request: &AssistantHistorySearchRequest,
    watermark: u64,
    now: u64,
    policy: &AssistantDataPolicy,
) -> bool {
    row.deleted_at_ms.is_none()
        && row.admitted_generation > 0
        && row.admitted_generation as u64 <= watermark
        && row.expires_at_ms > 0
        && row.expires_at_ms as u64 > now
        && row.recorded_at_ms > 0
        && row.recorded_at_ms as u64
            > now.saturating_sub(u64::from(policy.history_retention_days) * 86_400_000)
        && request
            .device_ref
            .as_ref()
            .is_none_or(|id| id == &row.device_ref)
        && request
            .conversation_id
            .as_ref()
            .is_none_or(|id| id == &row.root_conversation_id)
        && request.time_range.as_ref().is_none_or(|range| {
            row.recorded_at_ms as u64 >= range.from_unix_ms
                && (row.recorded_at_ms as u64) < range.until_unix_ms
        })
        && (request.source_kinds.is_empty()
            || serde_json::from_value(serde_json::Value::String(row.kind.clone()))
                .ok()
                .is_some_and(|kind| request.source_kinds.contains(&kind)))
}
fn visible(value: &desk_agent_protocol::ai_assistant::source::AssistantSourceResolution) -> bool {
    matches!(
        value.availability,
        SourceAvailability::Available | SourceAvailability::Partial | SourceAvailability::Redacted
    ) && value.content.is_some()
        && value.locator.is_some()
}

pub(crate) async fn search_on<C: ConnectionTrait + ConfigConnection>(
    db: &C,
    actor: &str,
    request: &AssistantHistorySearchRequest,
    policy: &AssistantDataPolicy,
    now: u64,
) -> Result<AssistantHistorySearchResult, AssistantStoreError> {
    request
        .validate_with_limits(&policy.limits.retrieval)
        .map_err(AssistantStoreError::Invalid)?;
    let began = Instant::now();
    let maximum = u64::from(policy.max_retrieval_document_visits)
        .min(u64::from(policy.limits.retrieval.owner_scan_documents));
    let mut key = request.clone();
    key.cursor = None;
    let request_hash = format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&(actor, key)).map_err(|_| AssistantStoreError::Inaccessible)?
        )
    );
    let runtime = runtime::Entity::find()
        .filter(runtime::Column::ActorId.eq(actor))
        .one(db)
        .await?
        .ok_or(AssistantStoreError::Inaccessible)?;
    let watermark = unsigned(runtime.history_data_generation)?;
    let mut cursor = if let Some(value) = &request.cursor {
        let raw = URL_SAFE_NO_PAD
            .decode(
                value
                    .strip_prefix("oh1_")
                    .ok_or(AssistantStoreError::Invalid("invalid history cursor"))?,
            )
            .map_err(|_| AssistantStoreError::Invalid("invalid history cursor"))?;
        if raw.len() > policy.limits.retrieval.episode.query_state_bytes as usize {
            return Err(AssistantStoreError::Invalid(
                "history cursor exceeds query state budget",
            ));
        }
        let cursor: Cursor = serde_json::from_slice(&raw)
            .map_err(|_| AssistantStoreError::Invalid("invalid history cursor"))?;
        if cursor.request_hash != request_hash
            || cursor.watermark > watermark
            || cursor.policy != policy.revision
        {
            return Err(AssistantStoreError::Invalid("history cursor scope changed"));
        }
        cursor
    } else {
        let groups = query_groups(&request.queries).map_err(AssistantStoreError::Invalid)?;
        let terms = groups.into_iter().flatten().collect::<BTreeSet<_>>();
        let mut query = history::Entity::find()
            .filter(history::Column::ActorId.eq(actor))
            .filter(history::Column::DeletedAtMs.is_null())
            .filter(history::Column::ExpiresAtMs.gt(signed(now)?))
            .filter(history::Column::AdmittedGeneration.lte(signed(watermark)?))
            .filter(history::Column::RecordedAtMs.gt(signed(
                now.saturating_sub(u64::from(policy.history_retention_days) * 86_400_000),
            )?));
        if let Some(device) = &request.device_ref {
            query = query.filter(history::Column::DeviceRef.eq(device));
        }
        if let Some(root) = &request.conversation_id {
            query = query.filter(history::Column::RootConversationId.eq(root));
        }
        if let Some(range) = &request.time_range {
            query = query
                .filter(history::Column::RecordedAtMs.gte(signed(range.from_unix_ms)?))
                .filter(history::Column::RecordedAtMs.lt(signed(range.until_unix_ms)?));
        }
        if !request.source_kinds.is_empty() {
            let kinds = request
                .source_kinds
                .iter()
                .map(|kind| {
                    serde_json::to_value(kind)
                        .unwrap()
                        .as_str()
                        .unwrap()
                        .to_string()
                })
                .collect::<Vec<_>>();
            query = query.filter(history::Column::Kind.is_in(kinds));
        }
        if !terms.is_empty() {
            let postings = term::Entity::find()
                .select_only()
                .column(term::Column::DocumentId)
                .filter(term::Column::ActorId.eq(actor))
                .filter(term::Column::Term.is_in(terms.iter().cloned()))
                .into_query();
            let docs = document::Entity::find()
                .select_only()
                .column(document::Column::SourceId)
                .filter(document::Column::ActorId.eq(actor))
                .filter(document::Column::SourceKind.eq("history"))
                .filter(document::Column::DocumentId.in_subquery(postings))
                .into_query();
            query = query.filter(history::Column::SourceId.in_subquery(docs));
        }
        let rows = query
            .order_by_desc(history::Column::RecordedAtMs)
            .order_by_desc(history::Column::SourceId)
            .limit(maximum + 1)
            .all(db)
            .await?;
        let mut reasons = BTreeSet::new();
        if rows.len() as u64 > maximum {
            reasons.insert("document_work_limit".to_string());
        }
        let mut candidates = Vec::new();
        let mut ranked = Vec::new();
        for row in rows.into_iter().take(maximum as usize) {
            if began.elapsed() >= Duration::from_millis(policy.limits.retrieval.owner_scan_ms) {
                reasons.insert("search_time_limit".into());
                break;
            }
            let resolved =
                super::owner_source::history_source_on(db, actor, &row, policy, now).await?;
            if !visible(&resolved) {
                reasons.insert("source_unavailable".into());
                continue;
            }
            if terms.is_empty() || request.sort == HistorySort::Time {
                candidates.push((row.source_id, row.revision));
                if candidates.len() == policy.limits.retrieval.query_candidates as usize {
                    reasons.insert("candidate_limit".into());
                    break;
                }
                continue;
            }
            let Some(doc) = document::Entity::find()
                .filter(document::Column::ActorId.eq(actor))
                .filter(document::Column::SourceKind.eq("history"))
                .filter(document::Column::SourceId.eq(&row.source_id))
                .filter(document::Column::SourceRevision.eq(row.revision))
                .one(db)
                .await?
            else {
                reasons.insert("source_unavailable".into());
                continue;
            };
            let postings = term::Entity::find()
                .filter(term::Column::ActorId.eq(actor))
                .filter(term::Column::DocumentId.eq(&doc.document_id))
                .filter(term::Column::SourceRevision.eq(row.revision))
                .filter(term::Column::Term.is_in(terms.iter().cloned()))
                .all(db)
                .await?;
            let mut frequencies = BTreeMap::new();
            for posting in postings {
                if posting.frequency > 0 {
                    frequencies.insert(posting.term, posting.frequency as u32);
                }
            }
            if frequencies.is_empty() || doc.token_count <= 0 {
                reasons.insert("source_unavailable".into());
                continue;
            }
            ranked.push(ReadableSearchDocument {
                document_id: doc.document_id,
                source_id: row.source_id,
                revision: unsigned(row.revision)?,
                root_conversation_id: row.root_conversation_id,
                token_count: doc.token_count as u32,
                frequencies,
            });
        }
        if !ranked.is_empty() {
            let result = rank_readable_documents(
                &request.queries,
                ranked,
                request.conversation_id.is_none(),
                &policy.limits.retrieval,
            )
            .map_err(AssistantStoreError::Invalid)?;
            candidates = result
                .documents
                .into_iter()
                .map(|hit| Ok((hit.document.source_id, signed(hit.document.revision)?)))
                .collect::<Result<Vec<_>, AssistantStoreError>>()?;
            reasons.insert("candidate_statistics".into());
        }
        Cursor {
            request_hash,
            watermark,
            policy: policy.revision,
            remaining: candidates,
            reasons: reasons.into_iter().collect(),
        }
    };
    let mut result = AssistantHistorySearchResult {
        fragments: Vec::new(),
        next_cursor: None,
        observed_through_generation: cursor.watermark,
        partial: cursor
            .reasons
            .iter()
            .any(|reason| reason != "candidate_statistics"),
        reasons: cursor.reasons.clone(),
    };
    let mut emitted = Vec::new();
    let mut visited = 0;
    while !cursor.remaining.is_empty() && result.fragments.len() < request.limit as usize {
        let reason = if visited >= maximum {
            Some("document_work_limit")
        } else if began.elapsed() >= Duration::from_millis(policy.limits.retrieval.owner_scan_ms) {
            Some("search_time_limit")
        } else {
            None
        };
        if let Some(reason) = reason {
            result.partial = true;
            if !result.reasons.iter().any(|value| value == reason) {
                result.reasons.push(reason.into());
            }
            break;
        }
        visited += 1;
        let (id, revision) = cursor.remaining[0].clone();
        let row = history::Entity::find()
            .filter(history::Column::ActorId.eq(actor))
            .filter(history::Column::SourceId.eq(&id))
            .one(db)
            .await?;
        let Some(row) = row.filter(|row| {
            row.revision == revision && matches(row, request, cursor.watermark, now, policy)
        }) else {
            cursor.remaining.remove(0);
            result.partial = true;
            continue;
        };
        let source = super::owner_source::history_source_on(db, actor, &row, policy, now).await?;
        if !visible(&source) {
            cursor.remaining.remove(0);
            result.partial = true;
            continue;
        }
        let kind = serde_json::from_value(serde_json::Value::String(row.kind.clone()))
            .map_err(|_| AssistantStoreError::Inaccessible)?;
        result.fragments.push(AssistantHistoryFragment {
            source,
            role: row.role.clone(),
            kind,
        });
        // Reserve the entire frozen continuation before fitting whole fragments.
        result.next_cursor =
            Some(cursor.encode(policy.limits.retrieval.episode.query_state_bytes)?);
        if serde_json::to_vec(&result)
            .map_err(|_| AssistantStoreError::Inaccessible)?
            .len()
            > policy.limits.conversation.page_bytes as usize
            && result.fragments.len() > 1
        {
            result.fragments.pop();
            break;
        }
        cursor.remaining.remove(0);
        emitted.push(row);
    }
    result.next_cursor = if cursor.remaining.is_empty() {
        None
    } else {
        Some(cursor.encode(policy.limits.retrieval.episode.query_state_bytes)?)
    };
    for (row, fragment) in emitted.iter().zip(&result.fragments) {
        if history::Entity::find_by_id(row.id).one(db).await?.as_ref() != Some(row)
            || super::owner_source::history_source_on(db, actor, row, policy, now).await?
                != fragment.source
        {
            return Err(AssistantStoreError::Inaccessible);
        }
    }
    if result.fragments.is_empty() && result.next_cursor.is_some() {
        return Err(AssistantStoreError::Capacity);
    }
    Ok(result)
}

pub(crate) async fn owner_history_search(
    db: &crate::config::connection::DatabaseConnection,
    request: &AssistantHistorySearchRequest,
    now: u64,
) -> Result<AssistantHistorySearchResult, AssistantStoreError> {
    use sea_orm::TransactionTrait;
    let txn = db.begin().await?;
    let actor = crate::control_authorizer::SINGLE_ACCOUNT_USER_ID.to_string();
    super::ensure_runtime_on(&txn, &actor).await?;
    let config = txn.config_read().await;
    let result = desk_diagnose_core::future::boxed(|| {
        search_on(&txn, &actor, request, &config.assistant_data_policy, now)
    })
    .await?;
    txn.commit().await?;
    Ok(result)
}
