//! Bounded revision selection. Hosts authorize every returned body and source.

use crate::entity::{agent_memory as memory, agent_memory_revision as revision};
use crate::memory_keyword::{MemoryReadAuthorization, MemorySourceReader, readable_statuses};
use crate::{AssistantStoreError, signed, unsigned};
use desk_agent_protocol::ai_assistant::history::AssistantDeviceScope;
use desk_agent_protocol::ai_assistant::memory::MemoryHistoryRequest;
use desk_diagnose_core::ai_assistant::{
    retrieval_query::{PageAnchor, QuerySourceVersion},
    retrieval_state::FrozenRetrievalQuery,
    seam::AssistantDataReadUse,
};
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect};
use std::time::{Duration, Instant};

pub async fn head_on<C: ConnectionTrait>(
    db: &C,
    context: &MemoryReadAuthorization<'_>,
    memory_id: &str,
) -> Result<memory::Model, AssistantStoreError> {
    let head = memory::Entity::find()
        .filter(memory::Column::ActorId.eq(context.actor_id))
        .filter(memory::Column::MemoryId.eq(memory_id))
        .one(db)
        .await?
        .ok_or(AssistantStoreError::Inaccessible)?;
    if head.revision <= 0
        || !readable_statuses(context).contains(&head.status.as_str())
        || head.state_generation <= 0
        || unsigned(head.state_generation)? > context.access.memory_state_generation
        || (!head.device_ref.is_empty()
            && (!context.requested_devices.contains(&head.device_ref)
                || (context.scope.device_rule == AssistantDeviceScope::CurrentDevice
                    && !context
                        .scope
                        .current_device_refs()
                        .contains(&head.device_ref))))
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    Ok(head)
}

pub struct RevisionPage {
    pub rows: Vec<revision::Model>,
    pub has_more: bool,
}

pub async fn page_on<C: ConnectionTrait>(
    db: &C,
    actor: &str,
    memory_id: &str,
    current_revision: u64,
    before_revision: Option<u64>,
    maximum: u32,
) -> Result<RevisionPage, AssistantStoreError> {
    let mut query = revision::Entity::find()
        .filter(revision::Column::ActorId.eq(actor))
        .filter(revision::Column::MemoryId.eq(memory_id))
        .filter(revision::Column::Revision.lte(signed(current_revision)?));
    if let Some(before) = before_revision {
        query = query.filter(revision::Column::Revision.lt(signed(before)?));
    }
    let mut rows = query
        .order_by_desc(revision::Column::Revision)
        .limit(u64::from(maximum) + 1)
        .all(db)
        .await?;
    let has_more = rows.len() > maximum as usize;
    rows.truncate(maximum as usize);
    Ok(RevisionPage { rows, has_more })
}

pub struct AuthorizedMemoryHistory {
    pub version: QuerySourceVersion,
    pub entry: desk_agent_protocol::ai_assistant::memory::MemoryHistoryEntry,
    pub usage: AssistantDataReadUse,
}

#[async_trait::async_trait]
pub trait MemoryHistoryReader<C: ConnectionTrait>: MemorySourceReader<C> {
    async fn read_version_on(
        db: &C,
        context: &MemoryReadAuthorization<'_>,
        version: &revision::Model,
    ) -> Result<Box<AuthorizedMemoryHistory>, AssistantStoreError>;
}

#[allow(clippy::vec_box)]
pub struct HistoryBatch {
    pub views: Vec<Box<AuthorizedMemoryHistory>>,
    pub frontiers: Vec<PageAnchor>,
    pub continuation: Option<PageAnchor>,
    pub more: bool,
    pub reasons: Vec<String>,
}

pub async fn scan_on<C: ConnectionTrait, R: MemoryHistoryReader<C>>(
    db: &C,
    context: &MemoryReadAuthorization<'_>,
    query: &FrozenRetrievalQuery,
    request: &MemoryHistoryRequest,
    current_revision: u64,
    page: Option<&PageAnchor>,
) -> Result<HistoryBatch, AssistantStoreError> {
    let before = match page {
        None | Some(PageAnchor::Start) => None,
        Some(PageAnchor::MemoryRevision { before_revision }) => Some(*before_revision),
        _ => return Err(AssistantStoreError::Inaccessible),
    };
    let started = Instant::now();
    let scan_budget =
        Duration::from_millis(context.platform.limits.retrieval.memory_keyword_scan_ms);
    let candidates = desk_diagnose_core::future::send_phase(|| {
        page_on(
            db,
            context.actor_id,
            &request.memory_id,
            current_revision,
            before,
            context.platform.limits.retrieval.query_candidates,
        )
    })
    .await?;
    let mut reasons = query.reasons.clone();
    let mut views = Vec::new();
    let mut frontiers = Vec::new();
    let mut last = before;
    let mut expected = before.unwrap_or(current_revision + 1);
    let mut more = candidates.has_more;
    let count = candidates.rows.len();
    for (index, row) in candidates.rows.iter().enumerate() {
        if started.elapsed() >= scan_budget {
            if last == before {
                return Err(AssistantStoreError::Capacity);
            }
            reasons.push("scan_budget".into());
            more = true;
            break;
        }
        let revision = unsigned(row.revision)?;
        if revision == 0 || revision >= expected {
            return Err(AssistantStoreError::Inaccessible);
        }
        if revision + 1 != expected {
            reasons.push("history_versions_unavailable".into());
        }
        expected = revision;
        last = Some(revision);
        let version = desk_diagnose_core::ai_assistant::retrieval_query::QuerySourceVersion {
            source_id: row.memory_id.clone(),
            revision,
            content_sha256: row.content_sha256.clone(),
        };
        if !query.was_seen(&version) {
            match R::read_version_on(db, context, row).await {
                Ok(view) => {
                    views.push(view);
                    frontiers.push(PageAnchor::MemoryRevision {
                        before_revision: revision,
                    });
                }
                Err(AssistantStoreError::Backend(error)) => return Err(error.into()),
                Err(_) => {
                    reasons.push("history_versions_unavailable".into());
                    R::validate_context_on(db, context).await?;
                    head_on(db, context, &request.memory_id).await?;
                }
            }
        }
        more = index + 1 < count || candidates.has_more;
        if views.len() >= request.limit as usize {
            break;
        }
    }
    if !more && expected > 1 {
        reasons.push("history_versions_unavailable".into());
    }
    if more && views.len() < request.limit as usize {
        reasons.push("scan_budget".into());
    }
    reasons.sort();
    reasons.dedup();
    Ok(HistoryBatch {
        views,
        frontiers,
        continuation: last
            .filter(|_| more)
            .map(|before_revision| PageAnchor::MemoryRevision { before_revision }),
        more,
        reasons,
    })
}
