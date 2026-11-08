//! Bounded temporal and original-window candidates. Results contain identities
//! only; final framed replies must reread sources and persist their read proofs.

use super::{
    AssistantStoreError, history_query,
    keyword_search::{self, HistorySearchAuthorization},
    unsigned,
};
use crate::entity::agent_assistant_history as history;
use desk_agent_protocol::ai_assistant::{
    history::{AssistantDeviceScope, HistoryTimeField},
    source::SourceReadPurpose,
};
use desk_diagnose_core::ai_assistant::{
    retrieval_query::{PageAnchor, QuerySourceVersion},
    retrieval_state::{FrozenRetrievalQuery, FrozenRetrievalRequest},
};
use sea_orm::{
    ConnectionTrait, FromQueryResult,
    sea_query::{Alias, Expr, SelectStatement},
};
use std::{
    collections::BTreeSet,
    time::{Duration, Instant},
};

pub(crate) mod time;
pub(crate) mod window;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct WindowPosition {
    pub admitted_generation: u64,
    pub fragment_index: u32,
    pub source_id: String,
}
impl WindowPosition {
    pub(crate) fn anchor(&self, forward: bool) -> PageAnchor {
        PageAnchor::Window {
            admitted_generation: self.admitted_generation,
            fragment_index: self.fragment_index,
            source_id: self.source_id.clone(),
            forward,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HistoryScanItem {
    pub source: QuerySourceVersion,
    pub position: WindowPosition,
    pub time_key_unix_ms: u64,
}

#[derive(FromQueryResult)]
struct ThinHistory {
    source_id: String,
    revision: i64,
    content_sha256: String,
    conversation_id: String,
    admitted_generation: i64,
    fragment_index: i32,
    recorded_at_ms: i64,
    native_action_at_ms: Option<i64>,
    native_action_from_ms: Option<i64>,
    native_action_until_ms: Option<i64>,
    time_key: i64,
}
impl ThinHistory {
    fn item(&self, observation: u64) -> Result<HistoryScanItem, AssistantStoreError> {
        let generation = unsigned(self.admitted_generation)?;
        let fragment_index =
            u32::try_from(self.fragment_index).map_err(|_| AssistantStoreError::Inaccessible)?;
        let revision = unsigned(self.revision)?;
        let time = unsigned(self.time_key)?;
        if generation == 0 || generation > observation || revision == 0 || time == 0 {
            return Err(AssistantStoreError::Inaccessible);
        }
        Ok(HistoryScanItem {
            source: QuerySourceVersion {
                source_id: self.source_id.clone(),
                revision,
            },
            position: WindowPosition {
                admitted_generation: generation,
                fragment_index,
                source_id: self.source_id.clone(),
            },
            time_key_unix_ms: time,
        })
    }
    fn same_row(&self, row: &history::Model) -> bool {
        self.source_id == row.source_id
            && self.revision == row.revision
            && self.content_sha256 == row.content_sha256
            && self.conversation_id == row.conversation_id
            && self.admitted_generation == row.admitted_generation
            && self.fragment_index == row.fragment_index
            && self.recorded_at_ms == row.recorded_at_ms
            && self.native_action_at_ms == row.native_action_at_ms
            && self.native_action_from_ms == row.native_action_from_ms
            && self.native_action_until_ms == row.native_action_until_ms
    }
    fn already_seen(&self, query: &FrozenRetrievalQuery, pending: &[HistoryScanItem]) -> bool {
        self.item(query.source_observation_generation)
            .is_ok_and(|item| {
                query.was_seen(&item.source)
                    || pending.iter().any(|prior| prior.source == item.source)
            })
    }
}

struct ScanWork {
    began: Instant,
    duration: Duration,
    maximum: usize,
    visited: usize,
    reasons: BTreeSet<&'static str>,
}
impl ScanWork {
    fn new(context: &HistorySearchAuthorization<'_>, duration: Duration) -> Self {
        Self {
            began: Instant::now(),
            duration,
            maximum: context.platform.max_retrieval_document_visits as usize,
            visited: 0,
            reasons: BTreeSet::new(),
        }
    }
    fn may_read(&mut self) -> bool {
        if self.began.elapsed() >= self.duration {
            self.reasons.insert("search_time_limit");
            return false;
        }
        if self.visited >= self.maximum {
            self.reasons.insert("document_work_limit");
            return false;
        }
        true
    }
    fn reasons(self) -> Vec<String> {
        self.reasons.into_iter().map(str::to_owned).collect()
    }
}

async fn devices_on<C: ConnectionTrait + crate::config::ConfigConnection>(
    db: &C,
    context: &HistorySearchAuthorization<'_>,
    query: &FrozenRetrievalQuery,
    page: Option<&PageAnchor>,
    owner_devices: &BTreeSet<String>,
) -> Result<(Vec<String>, bool), AssistantStoreError> {
    query.validate().map_err(AssistantStoreError::Invalid)?;
    keyword_search::validate_context_on(db, context).await?;
    if query.snapshot.purpose != SourceReadPurpose::ModelHistory
        || query.binding.actor_id != context.actor_id
        || query.binding.root_conversation_id != context.scope.root_conversation_id
        || query.binding.retrieval_episode_id != context.scope.retrieval_episode_id
        || query.binding.source_device_ref != context.current_device_ref
        || query.binding.scope_sha256 != context.scope_sha256
        || query.binding.history_scope.as_deref() != Some(context.scope)
        || &query.binding.destination != context.destination
        || query.snapshot.policy_revision != context.access.policy_revision
        || query.snapshot.control_generation != context.access.control_generation
        || query.snapshot.memory_generation != context.access.memory_state_generation
        || query.source_observation_generation != context.corpus_generation
        || context.now_unix_ms == 0
        || query.snapshot.expires_at_unix_ms <= context.now_unix_ms
        || page.is_some_and(|anchor| {
            !query
                .snapshot
                .continuations
                .iter()
                .any(|issued| &issued.anchor == anchor)
        })
    {
        return Err(AssistantStoreError::Inaccessible);
    }
    let (mut devices, changed) = query.snapshot.current_device_intersection(owner_devices);
    let rule = match &query.request {
        FrozenRetrievalRequest::Conversations { request } => request.scope,
        FrozenRetrievalRequest::Window { .. } => context.scope.device_rule,
        _ => return Err(AssistantStoreError::Inaccessible),
    };
    if rule == AssistantDeviceScope::CurrentDevice {
        devices.retain(|id| id == context.current_device_ref);
    }
    Ok((devices, changed))
}

fn columns(query: &mut SelectStatement, field: HistoryTimeField) {
    query
        .columns(
            [
                history::Column::SourceId,
                history::Column::Revision,
                history::Column::ContentSha256,
                history::Column::ConversationId,
                history::Column::AdmittedGeneration,
                history::Column::FragmentIndex,
                history::Column::RecordedAtMs,
                history::Column::NativeActionAtMs,
                history::Column::NativeActionFromMs,
                history::Column::NativeActionUntilMs,
            ]
            .map(|column| (history::Entity, column)),
        )
        .expr_as(
            history_query::time_expression(field),
            Alias::new("time_key"),
        );
}

async fn readable_on<C: ConnectionTrait + crate::config::ConfigConnection>(
    db: &C,
    context: &HistorySearchAuthorization<'_>,
    thin: &ThinHistory,
    work: &mut ScanWork,
) -> Result<Option<HistoryScanItem>, AssistantStoreError> {
    let item = match thin.item(context.corpus_generation) {
        Ok(item) => item,
        Err(_) => {
            work.reasons.insert("source_unavailable");
            return Ok(None);
        }
    };
    let result = keyword_search::readable_history_on(
        db,
        context,
        &item.source.source_id,
        item.source.revision,
    )
    .await;
    // A source error must not hide a concurrent switch, policy or control change.
    keyword_search::validate_context_on(db, context).await?;
    match result {
        Ok(source) if thin.same_row(&source.row) => Ok(Some(item)),
        Err(AssistantStoreError::Backend(error)) => Err(error.into()),
        _ => {
            work.reasons.insert("source_unavailable");
            Ok(None)
        }
    }
}
