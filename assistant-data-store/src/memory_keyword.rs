//! Bounded sparse fact retrieval. Statistics contain only currently readable
//! candidates; the final selected fact versions are read and authorized again.

use super::{AssistantStoreError, signed, unsigned};
use crate::entity::{
    agent_assistant_search_document as document, agent_assistant_search_term as term,
    agent_memory as memory,
};
use desk_agent_protocol::ai_assistant::{
    history::AssistantDeviceScope,
    memory::{MemorySearchRequest, MemoryUsePurpose},
};
use desk_diagnose_core::ai_assistant::{
    retrieval::query_groups,
    retrieval_query::QuerySourceVersion,
    retrieval_rank::{
        CANDIDATE_STATISTICS_REASON, ReadableSearchDocument, rank_readable_documents,
    },
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

use desk_agent_protocol::ai_assistant::user_instructions::AssistantDataPolicy;
use desk_agent_protocol::data_lineage::DestinationIdentity;
use desk_diagnose_core::ai_assistant::{
    data_access::AssistantDataToolAccess, memory_access::MemoryAccessScope,
};

/// Frozen original consent plus current host policy, never model arguments.
pub struct MemoryReadAuthorization<'a> {
    /// Separates independent control-decision read identities without changing
    /// original consent, retrieval episodes or source permissions. Superseded
    /// facts are historical references rather than relation targets.
    pub actor_id: &'a str,
    pub current_device_ref: &'a str,
    pub assistant_scope: &'a desk_diagnose_core::ai_assistant::data_access::AssistantScope,
    pub scope: &'a MemoryAccessScope,
    pub access: &'a AssistantDataToolAccess,
    pub platform: &'a AssistantDataPolicy,
    pub destination: &'a DestinationIdentity,
    pub requested_devices: &'a BTreeSet<String>,
    /// First query observation. Permission refresh cannot admit newer facts.
    pub observation_generation: u64,
    pub now_unix_ms: u64,
}

pub fn readable_statuses(_context: &MemoryReadAuthorization<'_>) -> &'static [&'static str] {
    &["active", "pending_conflict", "superseded"]
}

/// A candidate is usable only after the host validates its complete source closure.
#[async_trait::async_trait]
pub trait MemorySourceReader<C: ConnectionTrait> {
    async fn validate_context_on(
        db: &C,
        context: &MemoryReadAuthorization<'_>,
    ) -> Result<(), AssistantStoreError>;
    async fn authorize_version_on(
        db: &C,
        context: &MemoryReadAuthorization<'_>,
        version: &QuerySourceVersion,
        purpose: MemoryUsePurpose,
    ) -> Result<(), AssistantStoreError>;
}

const POSTING_PAGE: u64 = 128;

pub struct MemoryCandidates {
    pub versions: Vec<QuerySourceVersion>,
    pub reasons: Vec<String>,
}

#[derive(FromQueryResult)]
struct Posting {
    document_id: String,
    term: String,
    frequency: i32,
    source_revision: i64,
    source_id: String,
    token_count: i32,
}

pub async fn relevance_on<C: ConnectionTrait, R: MemorySourceReader<C>>(
    db: &C,
    context: &MemoryReadAuthorization<'_>,
    request: &MemorySearchRequest,
) -> Result<MemoryCandidates, AssistantStoreError> {
    scan_on::<C, R>(
        db,
        context,
        request,
        Duration::from_millis(context.platform.limits.retrieval.memory_keyword_scan_ms),
    )
    .await
}

pub async fn scan_on<C: ConnectionTrait, R: MemorySourceReader<C>>(
    db: &C,
    context: &MemoryReadAuthorization<'_>,
    request: &MemorySearchRequest,
    time_budget: Duration,
) -> Result<MemoryCandidates, AssistantStoreError> {
    R::validate_context_on(db, context).await?;
    if request.scope == AssistantDeviceScope::AllMyDevices
        && context.scope.device_rule != AssistantDeviceScope::AllMyDevices
    {
        return Err(AssistantStoreError::Inaccessible);
    }

    let terms = query_groups(&request.queries)
        .map_err(AssistantStoreError::Invalid)?
        .into_iter()
        .flatten()
        .collect::<BTreeSet<_>>();
    if terms.is_empty() {
        return Err(AssistantStoreError::Invalid(
            "keyword query has no searchable terms",
        ));
    }
    let maximum_documents = context.platform.max_retrieval_document_visits as usize;
    let maximum_postings = maximum_documents.saturating_mul(4);
    let mut cursor = None;
    let mut seen = BTreeSet::new();
    let mut authorized = BTreeSet::new();
    let mut readable = BTreeMap::<String, ReadableSearchDocument>::new();
    let mut reasons = BTreeSet::new();
    let began = Instant::now();
    let mut visited_postings = 0_usize;
    loop {
        if began.elapsed() >= time_budget {
            reasons.insert("search_time_limit");
            break;
        }
        if visited_postings >= maximum_postings {
            reasons.insert("index_work_limit");
            break;
        }
        let limit = std::cmp::Ord::min(POSTING_PAGE, (maximum_postings - visited_postings) as u64);
        let select = posting_query(context, request, &terms, cursor.as_ref(), limit)?;
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
            cursor = Some((posting.term.clone(), posting.document_id.clone()));
            visited_postings += 1;
            if !seen.contains(&posting.document_id) {
                if seen.len() >= maximum_documents {
                    reasons.insert("document_work_limit");
                    break;
                }
                seen.insert(posting.document_id.clone());
                let version = QuerySourceVersion {
                    source_id: posting.source_id.clone(),
                    revision: unsigned(posting.source_revision)?,
                };
                let identity = (version.source_id.clone(), version.revision);
                if !authorized.contains(&identity) {
                    let result = match desk_diagnose_core::future::boxed(|| {
                        R::authorize_version_on(
                            db,
                            context,
                            &version,
                            MemoryUsePurpose::HistoricalReference,
                        )
                    })
                    .await
                    {
                        Err(AssistantStoreError::Backend(error)) => return Err(error.into()),
                        result => result,
                    };
                    if began.elapsed() >= time_budget {
                        reasons.insert("search_time_limit");
                        break;
                    }
                    match result {
                        Ok(_) => {
                            authorized.insert(identity);
                        }
                        Err(AssistantStoreError::Backend(error)) => return Err(error.into()),
                        Err(_) => {
                            reasons.insert("source_unavailable");
                            continue;
                        }
                    }
                }
                if posting.token_count <= 0 {
                    reasons.insert("source_unavailable");
                    continue;
                }
                readable.insert(
                    posting.document_id.clone(),
                    ReadableSearchDocument {
                        document_id: posting.document_id.clone(),
                        source_id: posting.source_id.clone(),
                        revision: version.revision,
                        root_conversation_id: posting.source_id.clone(),
                        token_count: posting.token_count as u32,
                        frequencies: BTreeMap::new(),
                    },
                );
            }
            if let Some(doc) = readable.get_mut(&posting.document_id) {
                if doc.source_id != posting.source_id
                    || doc.revision != unsigned(posting.source_revision)?
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
    // A fact can have several index fragments. Aggregate those fragments once
    // before ranking, so one fact/version is one candidate and one DF document.
    let mut facts = BTreeMap::<(String, u64), ReadableSearchDocument>::new();
    for doc in readable.into_values() {
        if doc.frequencies.is_empty() {
            continue;
        }
        let key = (doc.source_id.clone(), doc.revision);
        if let Some(fact) = facts.get_mut(&key) {
            fact.token_count = fact
                .token_count
                .checked_add(doc.token_count)
                .ok_or(AssistantStoreError::Capacity)?;
            for (term, count) in doc.frequencies {
                let value = fact.frequencies.entry(term).or_default();
                *value = value
                    .checked_add(count)
                    .ok_or(AssistantStoreError::Capacity)?;
            }
        } else {
            facts.insert(
                key,
                ReadableSearchDocument {
                    document_id: doc.source_id.clone(),
                    ..doc
                },
            );
        }
    }
    if !facts.is_empty() {
        reasons.insert(CANDIDATE_STATISTICS_REASON);
    }
    let ranking = rank_readable_documents(
        &request.queries,
        facts.into_values().collect(),
        false,
        &context.platform.limits.retrieval,
    )
    .map_err(AssistantStoreError::Invalid)?;
    if ranking.statistics_document_count
        > context.platform.limits.retrieval.query_candidates as usize
    {
        reasons.insert("candidate_limit");
    }
    R::validate_context_on(db, context).await?;
    Ok(MemoryCandidates {
        versions: ranking
            .documents
            .into_iter()
            .map(|hit| QuerySourceVersion {
                source_id: hit.document.source_id,
                revision: hit.document.revision,
            })
            .collect(),
        reasons: reasons.into_iter().map(str::to_owned).collect(),
    })
}

fn allowed_devices(
    context: &MemoryReadAuthorization<'_>,
    request: &MemorySearchRequest,
) -> Vec<String> {
    // Account facts have no device selector and are always considered. Evidence
    // authorization still checks the real source device and original owner.
    std::iter::once(String::new())
        .chain(
            context
                .requested_devices
                .iter()
                .filter(|device| {
                    request.scope == AssistantDeviceScope::AllMyDevices
                        || context.scope.current_device_refs().contains(device)
                })
                .cloned(),
        )
        .collect()
}

fn posting_query(
    context: &MemoryReadAuthorization<'_>,
    request: &MemorySearchRequest,
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
            memory::Entity,
            Condition::all()
                .add(
                    Expr::col((document::Entity, document::Column::SourceId))
                        .equals((memory::Entity, memory::Column::MemoryId)),
                )
                .add(
                    Expr::col((document::Entity, document::Column::SourceRevision))
                        .equals((memory::Entity, memory::Column::Revision)),
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
            (document::Entity, document::Column::TokenCount),
        ])
        .and_where(Expr::col((term::Entity, term::Column::ActorId)).eq(context.actor_id))
        .and_where(Expr::col((document::Entity, document::Column::ActorId)).eq(context.actor_id))
        .and_where(Expr::col((memory::Entity, memory::Column::ActorId)).eq(context.actor_id))
        .and_where(Expr::col((document::Entity, document::Column::SourceKind)).eq("memory"))
        .and_where(
            Expr::col((document::Entity, document::Column::DeviceRef))
                .equals((memory::Entity, memory::Column::DeviceRef)),
        )
        .and_where(
            Expr::col((memory::Entity, memory::Column::DeviceRef))
                .is_in(allowed_devices(context, request)),
        )
        .and_where(
            Expr::col((document::Entity, document::Column::CorpusGeneration))
                .lte(signed(context.observation_generation)?),
        )
        .and_where(
            Expr::col((memory::Entity, memory::Column::StateGeneration))
                .lte(signed(context.observation_generation)?),
        )
        .and_where(
            Expr::col((document::Entity, document::Column::SourceRevision))
                .equals((term::Entity, term::Column::SourceRevision)),
        )
        .and_where(
            Expr::col((memory::Entity, memory::Column::Status))
                .is_in(readable_statuses(context).iter().copied()),
        )
        .cond_where(
            Condition::any()
                .add(Expr::col((document::Entity, document::Column::ExpiresAtMs)).is_null())
                .add(
                    Expr::col((document::Entity, document::Column::ExpiresAtMs))
                        .gt(signed(context.now_unix_ms)?),
                ),
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
    Ok(query.to_owned())
}
