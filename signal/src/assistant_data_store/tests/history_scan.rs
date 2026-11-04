use super::super::history_scan as scan;
use super::super::keyword_search as source_reads;
use super::keyword_search::{
    Frame, capture_at, capture_many_at, capture_many_native_at, device, private_source,
};
use super::model_sources::destination;
use super::*;
use crate::entity::{agent_assistant_history as history, agent_assistant_origin as origin};
use desk_agent_protocol::ai_assistant::history::{
    AssistantDeviceScope, ConversationReadRequest, ConversationReadView, ConversationSearchRequest,
    HistoryOrder, HistoryTimeField,
};
use desk_diagnose_core::{
    ai_assistant::{
        history::{RetrievalCallKind, RetrievalReservation},
        retrieval_query::{PageAnchor, QuerySourceVersion},
        retrieval_state::{
            FrozenRetrievalQuery, FrozenRetrievalRequest, RetrievalQueryBinding,
            RetrievalSelection, RetrievalWatermark,
        },
    },
    native_ids::{self, Kind},
};
use sea_orm::sea_query::Expr;
use std::{collections::BTreeSet, time::Duration};

mod time;
mod window;

fn temporal() -> ConversationSearchRequest {
    serde_json::from_value(serde_json::json!({"queries":[],"sort":"time","time_range":{"from_unix_ms":1,"until_unix_ms":4000},
        "scope":"all_my_devices","limit":50,"cursor":null})).unwrap()
}
fn prepare(
    frame: &Frame,
    request: FrozenRetrievalRequest,
    devices: &[String],
    logical: &str,
) -> FrozenRetrievalQuery {
    let kind = match &request {
        FrozenRetrievalRequest::Conversations { .. } => RetrievalCallKind::TimeEnumeration,
        FrozenRetrievalRequest::Window { .. } => RetrievalCallKind::Read,
        _ => unreachable!(),
    };
    let reservation = RetrievalReservation {
        logical_call_id: logical.into(),
        request_sha256: "b".repeat(64),
        kind,
        reserved_at_unix_ms: 4000,
        reserved_bytes: 32768,
        actual_bytes: None,
    };
    FrozenRetrievalQuery::prepare(
        RetrievalQueryBinding {
            actor_id: "1".into(),
            root_conversation_id: frame.scope.root_conversation_id.clone(),
            retrieval_episode_id: frame.scope.retrieval_episode_id.clone(),
            source_device_ref: frame.device.clone(),
            task_id: frame.scope.task_id.clone(),
            scope_sha256: frame.hash.clone(),
            history_scope: Some(Box::new(frame.scope.clone())),
            memory_device_rule: None,
            request_memory_device_refs: Vec::new(),
            destination: frame.destination.clone(),
            authority_expires_at_unix_ms: frame.scope.expires_at_unix_ms,
        },
        &reservation,
        request,
        RetrievalSelection {
            devices: devices.to_vec(),
            watermark: RetrievalWatermark {
                corpus_generation: frame.generation,
                memory_generation: frame.access.memory_state_generation,
                control_generation: frame.access.control_generation,
                policy_revision: frame.access.policy_revision,
            },
            candidates: Vec::new(),
            reasons: Vec::new(),
        },
        4500,
        desk_agent_protocol::ai_assistant::limits::RetrievalEpisodeLimits {
            query_ttl_ms: desk_agent_protocol::ai_assistant::limits::DEFAULT_QUERY_TTL_MS,
            ..Default::default()
        },
    )
    .unwrap()
}
async fn window_query(
    store: &TestStore,
    frame: &Frame,
    source: &str,
    view: ConversationReadView,
    limit: u32,
    devices: &[String],
    logical: &str,
) -> FrozenRetrievalQuery {
    let row = history::Entity::find()
        .filter(history::Column::SourceId.eq(source))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    prepare(
        frame,
        FrozenRetrievalRequest::Window {
            request: ConversationReadRequest {
                source_id: source.into(),
                view,
                limit,
                max_bytes: 32768,
                cursor: None,
            },
            anchor: QuerySourceVersion {
                source_id: source.into(),
                revision: row.revision as u64,
                content_sha256: row.content_sha256,
            },
            conversation_id: row.conversation_id,
        },
        devices,
        logical,
    )
}
fn accept(
    query: &mut FrozenRetrievalQuery,
    items: &[scan::HistoryScanItem],
    anchors: &[Option<PageAnchor>],
) {
    query
        .remember(
            &items
                .iter()
                .map(|item| item.source.clone())
                .collect::<Vec<_>>(),
        )
        .unwrap();
    for anchor in anchors.iter().flatten() {
        query
            .add_page(
                native_ids::stable(
                    Kind::AssistantPageCursor,
                    &format!(
                        "page-{}-{}",
                        query.snapshot.continuations.len(),
                        query.seen_sources.len()
                    ),
                ),
                anchor.clone(),
            )
            .unwrap();
    }
}
fn ids(items: &[scan::HistoryScanItem]) -> Vec<String> {
    items
        .iter()
        .map(|item| item.source.source_id.clone())
        .collect()
}
