use super::super::{conversation_search as search, retrieval_cache};
use super::keyword_search::{capture_at, private_source};
use super::retrieval_native::Frame;
use super::*;
use crate::entity::{
    agent_assistant_data_authorization as authorization,
    agent_assistant_read_projection as projection_row,
    agent_assistant_retrieval_query as query_row,
};
use desk_agent_protocol::ai_assistant::{
    history::ConversationSearchRequest, source::SourceReadPurpose,
};
use desk_diagnose_core::{
    ai_assistant::{
        data_access::scope_sha256,
        data_tools,
        history::serialized_tool_response_bytes,
        retrieval_state::{FrozenRetrievalQuery, RetrievalQueryBinding},
    },
    chat::ToolCall,
    native_ids::{self, Kind},
    session::PersistedAgentSession,
};
use sea_orm::PaginatorTrait;
use std::collections::BTreeSet;

fn request(frame: &Frame, index: usize) -> ConversationSearchRequest {
    serde_json::from_str(&frame.context_at(index).tool_call.arguments_json).unwrap()
}
async fn capture(frame: &Frame, root: &str, name: &str, text: &str, recorded: u64) -> String {
    capture_at(
        &frame.store,
        &frame
            .session
            .assistant_data
            .as_ref()
            .unwrap()
            .source_device_ref,
        root,
        name,
        text,
        &[],
        recorded,
    )
    .await
}

#[tokio::test]
async fn real_native_producer_searches_retained_history_and_replays_exact_body_without_another_query_charge()
 {
    let frame = Frame::new("search-flow-valid", true).await;
    let a = capture(
        &frame,
        "past-A",
        "search-A",
        "report 邮件 A",
        frame.now - 20,
    )
    .await;
    let b = capture(
        &frame,
        "past-B",
        "search-B",
        "report 文件 B",
        frame.now - 10,
    )
    .await;
    let before = quota::Entity::find()
        .one(&frame.store.db)
        .await
        .unwrap()
        .unwrap();
    let grants = authorization::Entity::find()
        .count(&frame.store.db)
        .await
        .unwrap();
    let original = search::search(&frame.store.db, frame.context(), &request(&frame, 0))
        .await
        .unwrap();
    assert_eq!(
        original
            .result
            .hits
            .iter()
            .map(|hit| hit.source_id.clone())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([a, b])
    );
    assert_eq!(
        original.projection.as_ref().unwrap().usage.purpose,
        SourceReadPurpose::ModelHistory
    );
    let body = serde_json::to_string(&original.result).unwrap();
    original
        .projection
        .as_ref()
        .unwrap()
        .validate_cached_result(&body)
        .unwrap();
    assert!(
        serialized_tool_response_bytes(&frame.context().tool_call.id, &body, None).unwrap()
            <= frame.context().maximum_response_bytes
    );
    let after = quota::Entity::find()
        .one(&frame.store.db)
        .await
        .unwrap()
        .unwrap();
    assert!(after.metadata_bytes > before.metadata_bytes);
    let query = retrieval_cache::decode_row(
        query_row::Entity::find()
            .one(&frame.store.db)
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(query.proof.seen_sources.len(), 2);
    capture(
        &frame,
        "past-C",
        "search-after-response",
        "report late addition",
        frame.now - 5,
    )
    .await;
    let before_replay = quota::Entity::find()
        .one(&frame.store.db)
        .await
        .unwrap()
        .unwrap();
    let replay = search::search(&frame.store.db, frame.context(), &request(&frame, 0))
        .await
        .unwrap();
    assert_eq!(replay.result, original.result);
    assert_eq!(replay.projection, original.projection);
    assert_eq!(
        quota::Entity::find()
            .one(&frame.store.db)
            .await
            .unwrap()
            .unwrap(),
        before_replay
    );
    assert_eq!(
        query_row::Entity::find()
            .count(&frame.store.db)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        authorization::Entity::find()
            .count(&frame.store.db)
            .await
            .unwrap(),
        grants
    );
}

// A single real accepted producer can contain several tool calls. The fixture
// supplies native opaque cursor hints before storing its exact output, so every
// page still passes the original producer/arguments/receipt/reservation checks.
fn time_calls(session: &PersistedAgentSession, from: u64, until: u64) -> Vec<ToolCall> {
    let data = session.assistant_data.as_ref().unwrap();
    let logical = |index: usize| {
        native_ids::stable(
            Kind::AssistantReadAuthorization,
            &(
                &session.actor_id,
                &session.conversation_id,
                &data.retrieval_budget.episode_id,
                "read-producer",
                &format!("time-call-{index}"),
            ),
        )
    };
    let binding = RetrievalQueryBinding {
        actor_id: session.actor_id.clone(),
        root_conversation_id: session.conversation_id.clone(),
        retrieval_episode_id: data.retrieval_budget.episode_id.clone(),
        source_device_ref: data.source_device_ref.clone(),
        task_id: None,
        scope_sha256: scope_sha256(session).unwrap(),
        history_scope: data.history_access_scope.clone(),
        memory_device_rule: None,
        request_memory_device_refs: Vec::new(),
        destination: super::model_sources::destination("model"),
        authority_expires_at_unix_ms: None,
    };
    let query = FrozenRetrievalQuery::query_id(&binding, &logical(0)).unwrap();
    (0..6).map(|index| ToolCall { id: format!("time-call-{index}"), name: data_tools::SEARCH_CONVERSATIONS.into(),
        arguments_json: serde_json::json!({"queries":["report"], "sort":"time", "order":"asc", "limit":50,
            "time_range":{"from_unix_ms":from,"until_unix_ms":until}, "source_kinds":["input"],
            "cursor":if index == 0 { None } else { Some(search::page_cursor(&query, &logical(index-1)).unwrap().1) }}).to_string() }).collect()
}

#[tokio::test]
async fn byte_limited_time_pages_resume_after_the_actual_last_output_and_do_not_admit_late_sources_after_refresh()
 {
    let clock = chrono::Utc::now().timestamp_millis() as u64;
    let frame = Frame::with_maximum("search-flow-time", true, Some(8192), |session| {
        time_calls(session, clock - 10_000, clock + 10_000)
    })
    .await;
    let mut expected = Vec::new();
    for index in 0..24 {
        expected.push(
            capture(
                &frame,
                "same-root",
                &format!("time-{index}"),
                &format!("report {index:02} {}", "邮件📧\\\"".repeat(90)),
                frame.now - 1000 + index,
            )
            .await,
        );
    }
    let mut emitted = Vec::new();
    let mut previous_cursor = None;
    let mut late = None;
    let mut ended = false;
    for index in 0..6 {
        let request = request(&frame, index);
        if index > 0 {
            assert_eq!(request.cursor, previous_cursor);
        }
        let page = search::search(&frame.store.db, frame.context_at(index), &request)
            .await
            .unwrap();
        let body = serde_json::to_string(&page.result).unwrap();
        assert!(
            serialized_tool_response_bytes(&frame.context_at(index).tool_call.id, &body, None)
                .unwrap()
                <= 8192
        );
        let proof = page.projection.as_ref().unwrap();
        proof.validate_cached_result(&body).unwrap();
        assert_eq!(proof.usage.roots.len(), page.result.hits.len());
        assert!(!page.result.hits.is_empty());
        if index == 0 {
            assert!(page.result.hits.len() < 24);
            assert!(page.result.has_more);
            assert!(
                page.result
                    .coverage
                    .reasons
                    .iter()
                    .any(|reason| reason == "response_byte_limit")
            );
            late = Some(
                capture(
                    &frame,
                    "same-root",
                    "late-before-frontier",
                    "report appeared after the frozen observation",
                    frame.now - 999,
                )
                .await,
            );
        }
        emitted.extend(page.result.hits.iter().map(|hit| hit.source_id.clone()));
        previous_cursor = page.result.next_cursor.clone();
        if !page.result.has_more {
            ended = true;
            break;
        }
    }
    assert!(ended);
    assert_eq!(emitted, expected);
    assert!(!emitted.contains(late.as_ref().unwrap()));
    let query = retrieval_cache::decode_row(
        query_row::Entity::find()
            .one(&frame.store.db)
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(query.proof.seen_sources.len(), expected.len());
    assert!(query.proof.snapshot.corpus_generation > query.proof.source_observation_generation);
}

#[tokio::test]
async fn late_query_cache_failure_rolls_back_the_response_proof_seen_ledger_and_exact_quota() {
    let frame = Frame::new("search-flow-rollback", true).await;
    capture(
        &frame,
        "past-A",
        "rollback-source",
        "report rollback",
        frame.now - 20,
    )
    .await;
    let before = quota::Entity::find()
        .one(&frame.store.db)
        .await
        .unwrap()
        .unwrap();
    let projections = projection_row::Entity::find()
        .count(&frame.store.db)
        .await
        .unwrap();
    frame.store.db.execute_unprepared("CREATE TRIGGER reject_final_query BEFORE INSERT ON agent_assistant_retrieval_query BEGIN SELECT RAISE(ABORT, 'injected final cache failure'); END").await.unwrap();
    assert!(
        search::search(&frame.store.db, frame.context(), &request(&frame, 0))
            .await
            .is_err()
    );
    assert_eq!(
        query_row::Entity::find()
            .count(&frame.store.db)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        projection_row::Entity::find()
            .count(&frame.store.db)
            .await
            .unwrap(),
        projections
    );
    assert_eq!(
        quota::Entity::find()
            .one(&frame.store.db)
            .await
            .unwrap()
            .unwrap(),
        before
    );
}

#[tokio::test]
async fn private_sources_produce_an_empty_but_proven_result_and_unaccepted_or_changed_calls_do_not_query()
 {
    let frame = Frame::new("search-flow-private", true).await;
    let source = capture(
        &frame,
        "past-A",
        "private-source",
        "report private",
        frame.now - 20,
    )
    .await;
    private_source(&frame.store, &source).await;
    let result = search::search(&frame.store.db, frame.context(), &request(&frame, 0))
        .await
        .unwrap();
    assert!(result.result.hits.is_empty());
    assert!(result.projection.as_ref().unwrap().usage.roots.is_empty());
    assert!(result.result.coverage.partial && result.result.coverage.has_gaps);
    let frame = Frame::new("search-flow-unaccepted", false).await;
    assert!(
        search::search(&frame.store.db, frame.context(), &request(&frame, 0))
            .await
            .is_err()
    );
    let frame = Frame::new("search-flow-argument-change", true).await;
    let mut altered = request(&frame, 0);
    altered.queries = vec!["different".into()];
    frame
        .change_persisted_arguments(&serde_json::to_string(&altered).unwrap())
        .await;
    assert!(
        search::search(&frame.store.db, frame.context(), &request(&frame, 0))
            .await
            .is_err()
    );
    assert_eq!(
        query_row::Entity::find()
            .count(&frame.store.db)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn missing_delivered_response_proof_does_not_turn_the_same_logical_call_into_another_scan() {
    let frame = Frame::new("search-flow-missing-proof", true).await;
    capture(
        &frame,
        "past-A",
        "original-source",
        "report original",
        frame.now - 20,
    )
    .await;
    search::search(&frame.store.db, frame.context(), &request(&frame, 0))
        .await
        .unwrap();
    let original = query_row::Entity::find()
        .one(&frame.store.db)
        .await
        .unwrap()
        .unwrap();
    let query = retrieval_cache::decode_row(original.clone()).unwrap();
    assert!(query.proof.was_delivered(frame.context().logical_call_id));
    // Simulate a missing result independently of the still-live query cache.
    // Native producer/source proofs remain present; they cannot reconstruct the
    // original response from a now advanced delivered-version ledger.
    projection_row::Entity::delete_many()
        .filter(projection_row::Column::LogicalCallId.eq(frame.context().logical_call_id))
        .exec(&frame.store.db)
        .await
        .unwrap();
    capture(
        &frame,
        "past-B",
        "new-source",
        "report added after delivery",
        frame.now - 10,
    )
    .await;
    let quota_before = quota::Entity::find()
        .one(&frame.store.db)
        .await
        .unwrap()
        .unwrap();
    let count_before = projection_row::Entity::find()
        .count(&frame.store.db)
        .await
        .unwrap();
    assert!(
        search::search(&frame.store.db, frame.context(), &request(&frame, 0))
            .await
            .is_err()
    );
    assert_eq!(
        query_row::Entity::find()
            .one(&frame.store.db)
            .await
            .unwrap()
            .unwrap(),
        original
    );
    assert_eq!(
        projection_row::Entity::find()
            .count(&frame.store.db)
            .await
            .unwrap(),
        count_before
    );
    assert_eq!(
        quota::Entity::find()
            .one(&frame.store.db)
            .await
            .unwrap()
            .unwrap(),
        quota_before
    );
}

#[tokio::test]
async fn native_inventory_search_corpus_binds_twenty_roots_and_five_statuses_to_actual_returned_sources()
 {
    use desk_diagnose_core::ai_assistant::native_history::{
        NativeHistorySnapshot, NativeHistoryStatus, NativeHistoryTaskKind,
        NativeStatusHistoryProjection,
    };
    use desk_diagnose_core::ai_assistant::source::HistorySourceDirectory;
    for maximum in [32768, 8192] {
        let clock = chrono::Utc::now().timestamp_millis() as u64;
        let frame = Frame::with_maximum("native-inventory-search", true, Some(maximum), |_| vec![ToolCall {
            id: "inventory-search".into(), name: data_tools::SEARCH_CONVERSATIONS.into(), arguments_json: serde_json::json!({
                "queries":["report"],"sort":"time","order":"asc","limit":20,"source_kinds":["input"],
                "time_range":{"from_unix_ms":clock-10000,"until_unix_ms":clock+10000}
            }).to_string() }]).await;
        let data = frame.session.assistant_data.as_ref().unwrap();
        let mut status_sources = std::collections::BTreeMap::new();
        for index in 0..20 {
            let root = format!("inventory-candidate-{index}");
            capture(
                &frame,
                &root,
                &format!("inventory-input-{index}"),
                &format!("report candidate {index}"),
                frame.now - 1000 + index,
            )
            .await;
            if index % 4 == 0 {
                // Fixed retained-state corpus exercises retrieval; the separate
                // native producer test proves actual work row collection.
                let items = if index == 0 {
                    Vec::new()
                } else {
                    (0..4)
                        .map(|task| NativeHistoryStatus {
                            native_id: format!("native-{index}-{task}"),
                            kind: NativeHistoryTaskKind::Work,
                            state: if task == 1 {
                                "unknown"
                            } else if task == 2 {
                                "dispatched"
                            } else {
                                "done"
                            }
                            .into(),
                            revision: None,
                            observed_at_unix_ms: frame.now - 500,
                        })
                        .collect()
                };
                let snapshot = NativeHistorySnapshot::bounded(
                    items,
                    true,
                    frame.now - 500,
                    desk_agent_protocol::ai_assistant::limits::AssistantProcessingLimits::default()
                        .native_status_items as usize,
                )
                .unwrap();
                let projection = NativeStatusHistoryProjection::new(
                    &frame.session.actor_id,
                    &data.source_device_ref,
                    &root,
                    &format!("{index:064x}"),
                    snapshot,
                    1,
                )
                .unwrap();
                let directory =
                    data.source_instance_id
                        .as_deref()
                        .map(|instance_id| HistorySourceDirectory {
                            instance_id,
                            audience: &frame.session.device_id,
                            device_name: None,
                        });
                let txn = frame.store.db.begin().await.unwrap();
                crate::assistant_data_store::history::capture_on(
                    &txn,
                    &projection.capture(data.current_actor_input_seq, directory),
                    &AssistantDataPolicy::default(),
                )
                .await
                .unwrap();
                txn.commit().await.unwrap();
                status_sources.insert(root, projection.source_id());
            }
        }
        let page = search::search(&frame.store.db, frame.context(), &request(&frame, 0))
            .await
            .unwrap();
        let summaries = page
            .result
            .hits
            .iter()
            .filter_map(|hit| hit.native_status_summary.as_ref())
            .collect::<Vec<_>>();
        if maximum == 32768 {
            assert_eq!(page.result.hits.len(), 20);
            assert_eq!(summaries.len(), 5);
        } else {
            assert!(page.result.hits.len() < 20);
            assert!(page.result.has_more);
        }
        let mut expected = BTreeSet::new();
        for hit in &page.result.hits {
            expected.insert(hit.source_id.clone());
            if let Some(summary) = &hit.native_status_summary {
                expected.insert(status_sources[&hit.locator.root_conversation_id].clone());
                assert_eq!(summary.last_recorded_at_unix_ms, frame.now - 500);
                if summary.no_native_task {
                    assert!(summary.items.is_empty());
                } else {
                    assert_eq!(summary.unknown_count, 1);
                    assert_eq!(summary.pending_count, 2);
                    assert_eq!(summary.additional_count, 1);
                    assert_eq!(summary.items[0].state, "unknown");
                    assert!(summary.items.iter().all(|item| item.historical
                        && item.revision.is_none()
                        && item.source_device.as_ref() == Some(&hit.source_device)));
                }
            }
        }
        let proof = page.projection.as_ref().unwrap();
        assert_eq!(
            proof
                .usage
                .roots
                .iter()
                .map(|root| root.source_id.clone())
                .collect::<BTreeSet<_>>(),
            expected
        );
        let query = retrieval_cache::decode_row(
            query_row::Entity::find()
                .one(&frame.store.db)
                .await
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            query
                .proof
                .seen_sources
                .iter()
                .map(|source| source.source_id.clone())
                .collect::<BTreeSet<_>>(),
            expected
        );
        proof
            .validate_cached_result(&serde_json::to_string(&page.result).unwrap())
            .unwrap();
    }
}

#[tokio::test]
async fn native_inventory_retained_completion_survives_an_empty_live_inventory_without_execution_rows()
 {
    use desk_diagnose_core::ai_assistant::{
        native_history::{
            NativeHistorySnapshot, NativeHistoryStatus, NativeHistoryTaskKind,
            NativeStatusHistoryProjection,
        },
        source::HistorySourceDirectory,
    };
    let frame = Frame::new("native-inventory-reclaimed", true).await;
    let data = frame.session.assistant_data.as_ref().unwrap();
    capture(
        &frame,
        "retained-native-root",
        "retained-native-input",
        "report task",
        frame.now - 1000,
    )
    .await;
    let mut ids = Vec::new();
    for (index, items) in [
        (
            0,
            vec![NativeHistoryStatus {
                native_id: "retained-goal".into(),
                kind: NativeHistoryTaskKind::Goal,
                state: "completed".into(),
                revision: Some(7),
                observed_at_unix_ms: frame.now - 100,
            }],
        ),
        (1, Vec::new()),
    ] {
        let inventory = NativeHistorySnapshot::bounded(
            items,
            true,
            frame.now - 100 + index,
            desk_agent_protocol::ai_assistant::limits::AssistantProcessingLimits::default()
                .native_status_items as usize,
        )
        .unwrap();
        let projection = NativeStatusHistoryProjection::new(
            &frame.session.actor_id,
            &data.source_device_ref,
            "retained-native-root",
            &format!("{index:064x}"),
            inventory,
            1,
        )
        .unwrap();
        let directory =
            data.source_instance_id
                .as_deref()
                .map(|instance_id| HistorySourceDirectory {
                    instance_id,
                    audience: &frame.session.device_id,
                    device_name: None,
                });
        let txn = frame.store.db.begin().await.unwrap();
        crate::assistant_data_store::history::capture_on(
            &txn,
            &projection.capture(data.current_actor_input_seq, directory),
            &AssistantDataPolicy::default(),
        )
        .await
        .unwrap();
        txn.commit().await.unwrap();
        ids.push(projection.source_id());
    }
    let result = search::search(&frame.store.db, frame.context(), &request(&frame, 0))
        .await
        .unwrap();
    assert_eq!(result.result.hits.len(), 1);
    let summary = result.result.hits[0]
        .native_status_summary
        .as_ref()
        .unwrap();
    assert_eq!(summary.source_id, ids[1]);
    assert_eq!(summary.additional_sources[0].id, ids[0]);
    assert!(!summary.no_native_task);
    assert!(!summary.has_more);
    assert_eq!(summary.items[0].native_id, "retained-goal");
    assert_eq!(summary.items[0].source_id, ids[0]);
    assert_eq!(summary.items[0].state, "completed");
    assert_eq!(summary.items[0].revision, Some(7));
    assert!(summary.items[0].historical);
    assert_eq!(summary.items[0].observed_at_unix_ms, frame.now - 100);
    assert_eq!(result.projection.as_ref().unwrap().usage.roots.len(), 3);
    assert_eq!(
        crate::entity::agent_session::Entity::find()
            .filter(crate::entity::agent_session::Column::ConversationId.eq("retained-native-root"))
            .count(&frame.store.db)
            .await
            .unwrap(),
        0
    );
}

use crate::entity::agent_session;
include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../signal-facade/tests/fixtures/conversation_alias_search.rs"
));
