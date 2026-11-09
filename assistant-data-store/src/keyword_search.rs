//! Relevance candidates from bounded, owner-filtered inverted-index pages.
//! Caller holds the original actor/read transaction; selected bodies are read
//! and authorized again before a durable result proof or native send is created.

use super::{AssistantStoreError, signed, unsigned};
use crate::entity::{
    agent_assistant_history as history, agent_assistant_search_document as document,
    agent_assistant_search_term as term,
};
use desk_agent_protocol::{
    ai_assistant::{
        history::{
            AssistantDeviceScope, ConversationSearchRequest, HistoryAccessScope, HistorySort,
            HistoryTimeField,
        },
        user_instructions::AssistantDataPolicy,
    },
    data_lineage::DestinationIdentity,
};
use desk_diagnose_core::ai_assistant::{
    data_access::AssistantDataToolAccess,
    history::validate_search_scope,
    retrieval::query_groups,
    retrieval_rank::{
        CANDIDATE_STATISTICS_REASON, RankedSearchCandidates, ReadableSearchDocument,
        rank_readable_documents,
    },
    seam::AssistantDataReadUse,
};
use sea_orm::ExprTrait;
use sea_orm::{
    ConnectionTrait, FromQueryResult,
    sea_query::{Condition, Expr, Order, Query, SelectStatement},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::{Duration, Instant},
};

/// A candidate must pass the host's original native/source checks before ranking.
#[async_trait::async_trait]
pub trait HistorySourceReader<C: ConnectionTrait> {
    async fn validate_context_on(
        db: &C,
        context: &HistorySearchAuthorization<'_>,
    ) -> Result<(), AssistantStoreError>;
    async fn readable_history_on(
        db: &C,
        context: &HistorySearchAuthorization<'_>,
        source_id: &str,
        revision: u64,
    ) -> Result<Box<ReadableHistorySource>, AssistantStoreError>;
}

const POSTING_PAGE: u64 = 128;

/// Host-created, never accepted from model arguments or owner-view responses.
pub struct HistorySearchAuthorization<'a> {
    pub actor_id: &'a str,
    pub current_device_ref: &'a str,
    pub assistant_scope: &'a desk_diagnose_core::ai_assistant::data_access::AssistantScope,
    pub scope: &'a HistoryAccessScope,
    pub access: &'a AssistantDataToolAccess,
    pub platform: &'a AssistantDataPolicy,
    pub destination: &'a DestinationIdentity,
    /// Original query observation, not its newer permission-refresh watermark.
    pub corpus_generation: u64,
    pub now_unix_ms: u64,
}

pub struct KeywordCandidates {
    pub ranking: RankedSearchCandidates,
    pub reasons: Vec<String>,
}

pub struct ReadableHistorySource {
    pub row: history::Model,
    pub usage: AssistantDataReadUse,
}

impl ReadableHistorySource {
    pub fn version(
        &self,
    ) -> Result<
        desk_diagnose_core::ai_assistant::retrieval_query::QuerySourceVersion,
        AssistantStoreError,
    > {
        Ok(
            desk_diagnose_core::ai_assistant::retrieval_query::QuerySourceVersion {
                source_id: self.row.source_id(),
                revision: unsigned(self.row.revision)?,
            },
        )
    }
}

#[derive(FromQueryResult)]
struct Posting {
    document_id: String,
    term: String,
    frequency: i32,
    source_revision: i64,
    source_id: String,
    root_conversation_id: String,
    token_count: i32,
}

pub async fn relevance_on<C: ConnectionTrait, R: HistorySourceReader<C>>(
    db: &C,
    context: &HistorySearchAuthorization<'_>,
    request: &ConversationSearchRequest,
    owner_devices: &BTreeSet<String>,
) -> Result<KeywordCandidates, AssistantStoreError> {
    scan_on::<C, R>(
        db,
        context,
        request,
        owner_devices,
        Duration::from_millis(context.platform.limits.retrieval.history_keyword_scan_ms),
    )
    .await
}

pub async fn scan_on<C: ConnectionTrait, R: HistorySourceReader<C>>(
    db: &C,
    context: &HistorySearchAuthorization<'_>,
    request: &ConversationSearchRequest,
    owner_devices: &BTreeSet<String>,
    time_budget: Duration,
) -> Result<KeywordCandidates, AssistantStoreError> {
    R::validate_context_on(db, context).await?;
    validate_search_scope(context.scope, request, context.now_unix_ms)
        .map_err(AssistantStoreError::Invalid)?;
    if request.sort != HistorySort::Relevance {
        return Err(AssistantStoreError::Invalid(
            "time enumeration does not use relevance ranking",
        ));
    }
    if owner_devices.len() > context.platform.limits.retrieval.episode.query_devices as usize {
        return Err(AssistantStoreError::Capacity);
    }
    let mut groups = query_groups(&request.queries).map_err(AssistantStoreError::Invalid)?;
    groups.sort();
    let terms = groups.iter().flatten().cloned().collect::<BTreeSet<_>>();
    if terms.is_empty() {
        return Err(AssistantStoreError::Invalid(
            "keyword query has no searchable terms",
        ));
    }
    let devices = owner_devices
        .iter()
        .filter(|id| {
            request.scope == AssistantDeviceScope::AllMyDevices
                || id.as_str() == context.current_device_ref
        })
        .cloned()
        .collect::<Vec<_>>();
    let maximum_documents = context.platform.max_retrieval_document_visits as usize;
    let maximum_postings = maximum_documents.saturating_mul(4);
    let began = Instant::now();
    let mut cursor: Option<(String, String)> = None;
    let mut seen = BTreeSet::new();
    let mut readable = BTreeMap::<String, ReadableSearchDocument>::new();
    let mut reasons = BTreeSet::new();
    let mut visited_postings = 0_usize;
    while !devices.is_empty() {
        if began.elapsed() >= time_budget {
            reasons.insert("search_time_limit");
            break;
        }
        if visited_postings >= maximum_postings {
            reasons.insert("index_work_limit");
            break;
        }
        let limit = std::cmp::Ord::min(POSTING_PAGE, (maximum_postings - visited_postings) as u64);
        let select = posting_query(context, request, &devices, &terms, cursor.as_ref(), limit)?;
        let rows = Posting::find_by_statement(db.get_database_backend().build(&select))
            .all(db)
            .await?;
        if began.elapsed() >= time_budget {
            reasons.insert("search_time_limit");
            break;
        }
        let finished = rows.len() < limit as usize;
        for posting in rows {
            if began.elapsed() >= time_budget {
                reasons.insert("search_time_limit");
                break;
            }
            visited_postings += 1;
            cursor = Some((posting.term.clone(), posting.document_id.clone()));
            if !seen.contains(&posting.document_id) {
                if seen.len() >= maximum_documents {
                    reasons.insert("document_work_limit");
                    break;
                }
                seen.insert(posting.document_id.clone());
                let source = match R::readable_history_on(
                    db,
                    context,
                    &posting.source_id,
                    unsigned(posting.source_revision)?,
                )
                .await
                {
                    Err(AssistantStoreError::Backend(error)) => return Err(error.into()),
                    result => result,
                };
                if began.elapsed() >= time_budget {
                    reasons.insert("search_time_limit");
                    break;
                }
                match source {
                    Ok(source) => {
                        if posting.root_conversation_id != source.row.root_conversation_id
                            || posting.token_count <= 0
                        {
                            reasons.insert("source_unavailable");
                            continue;
                        }
                        if request.time_field == HistoryTimeField::NativeActionAt
                            && source.row.native_action_at_ms.is_none()
                            && source
                                .row
                                .native_action_from_ms
                                .zip(source.row.native_action_until_ms)
                                .is_some_and(|(from, until)| from > 0 && until > from)
                        {
                            reasons.insert("native_action_time_interval");
                        }
                        readable.insert(
                            posting.document_id.clone(),
                            ReadableSearchDocument {
                                document_id: posting.document_id.clone(),
                                source_id: source.row.source_id(),
                                revision: unsigned(source.row.revision)?,
                                root_conversation_id: source.row.root_conversation_id,
                                token_count: posting.token_count as u32,
                                frequencies: BTreeMap::new(),
                            },
                        );
                    }
                    Err(_) => {
                        reasons.insert("source_unavailable");
                        continue;
                    }
                }
            }
            if let Some(doc) = readable.get_mut(&posting.document_id) {
                if doc.source_id != posting.source_id
                    || doc.revision != unsigned(posting.source_revision)?
                    || doc.root_conversation_id != posting.root_conversation_id
                    || posting.token_count <= 0
                    || doc.token_count != posting.token_count as u32
                    || posting.frequency <= 0
                    || posting.frequency as u32 > doc.token_count
                {
                    readable.remove(&posting.document_id);
                    reasons.insert("source_unavailable");
                    continue;
                }
                if let Some(previous) = doc
                    .frequencies
                    .insert(posting.term, posting.frequency as u32)
                    && previous != posting.frequency as u32
                {
                    readable.remove(&posting.document_id);
                    reasons.insert("source_unavailable");
                }
            }
        }
        if finished
            || reasons.contains("search_time_limit")
            || reasons.contains("document_work_limit")
        {
            break;
        }
    }
    if !readable.is_empty() {
        reasons.insert(CANDIDATE_STATISTICS_REASON);
    }
    let ranking = rank_readable_documents(
        &request.queries,
        readable.into_values().collect(),
        request.conversation_ids.is_empty(),
        &context.platform.limits.retrieval,
    )
    .map_err(AssistantStoreError::Invalid)?;
    Ok(KeywordCandidates {
        ranking,
        reasons: reasons.into_iter().map(str::to_owned).collect(),
    })
}

fn posting_query(
    context: &HistorySearchAuthorization<'_>,
    request: &ConversationSearchRequest,
    devices: &[String],
    terms: &BTreeSet<String>,
    cursor: Option<&(String, String)>,
    limit: u64,
) -> Result<SelectStatement, AssistantStoreError> {
    let mut query = Query::select();
    query
        .from(term::Entity)
        .inner_join(
            document::Entity,
            Expr::col((term::Entity, term::Column::DocumentId))
                .equals((document::Entity, document::Column::DocumentId)),
        )
        .inner_join(
            history::Entity,
            Condition::all()
                .add(
                    Expr::col((document::Entity, document::Column::HistoryId))
                        .equals((history::Entity, history::Column::Id)),
                )
                .add(
                    Expr::col((document::Entity, document::Column::SourceRevision))
                        .equals((history::Entity, history::Column::Revision)),
                ),
        )
        .columns([
            (term::Entity, term::Column::DocumentId),
            (term::Entity, term::Column::Term),
            (term::Entity, term::Column::Frequency),
            (term::Entity, term::Column::SourceRevision),
        ])
        .columns([
            (document::Entity, document::Column::SourceId),
            (document::Entity, document::Column::RootConversationId),
            (document::Entity, document::Column::TokenCount),
        ])
        .and_where(Expr::col((term::Entity, term::Column::ActorId)).eq(context.actor_id))
        .and_where(Expr::col((document::Entity, document::Column::ActorId)).eq(context.actor_id))
        .and_where(Expr::col((history::Entity, history::Column::ActorId)).eq(context.actor_id))
        .and_where(Expr::col((document::Entity, document::Column::SourceKind)).eq("history"))
        .and_where(
            Expr::col((document::Entity, document::Column::DeviceRef))
                .is_in(devices.iter().cloned()),
        )
        .and_where(
            Expr::col((document::Entity, document::Column::CorpusGeneration))
                .lte(signed(context.corpus_generation)?),
        )
        .and_where(
            Expr::col((document::Entity, document::Column::SourceRevision))
                .equals((term::Entity, term::Column::SourceRevision)),
        )
        .and_where(
            Expr::col((document::Entity, document::Column::DeviceRef))
                .equals((history::Entity, history::Column::DeviceRef)),
        )
        .and_where(Expr::col((history::Entity, history::Column::DeletedAtMs)).is_null())
        .and_where(
            Expr::col((history::Entity, history::Column::ExpiresAtMs))
                .gt(signed(context.now_unix_ms)?),
        )
        .and_where(
            Expr::col((history::Entity, history::Column::RecordedAtMs)).gt(signed(
                context.now_unix_ms.saturating_sub(
                    u64::from(context.platform.history_retention_days) * 86_400_000,
                ),
            )?),
        )
        .and_where(Expr::col((term::Entity, term::Column::Term)).is_in(terms.iter().cloned()))
        .order_by((term::Entity, term::Column::Term), Order::Asc)
        .order_by((term::Entity, term::Column::DocumentId), Order::Asc)
        .limit(limit);
    if let Some((last_term, last_document)) = cursor {
        query.cond_where(
            Condition::any()
                .add(Expr::col((term::Entity, term::Column::Term)).gt(last_term.clone()))
                .add(
                    Condition::all()
                        .add(Expr::col((term::Entity, term::Column::Term)).eq(last_term.clone()))
                        .add(
                            Expr::col((term::Entity, term::Column::DocumentId))
                                .gt(last_document.clone()),
                        ),
                ),
        );
    }
    super::history_query::apply_scope(&mut query, context)?;
    super::history_query::apply_search(&mut query, context, request)?;
    Ok(query.to_owned())
}
