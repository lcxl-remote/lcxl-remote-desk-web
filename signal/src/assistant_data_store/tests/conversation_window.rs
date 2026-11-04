use super::super::{conversation_search, conversation_window, retrieval_cache};
use super::keyword_search::{capture_at, private_source};
use super::retrieval_native::Frame;
use super::*;
use crate::entity::{
    agent_assistant_data_authorization as authorization, agent_assistant_history as history_row,
    agent_assistant_read_projection as projection_row,
    agent_assistant_retrieval_query as query_row,
};
use desk_agent_protocol::ai_assistant::{
    history::{ConversationReadRequest, ConversationSearchRequest},
    source::SourceReadPurpose,
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

fn request(frame: &Frame, index: usize) -> ConversationReadRequest {
    serde_json::from_str(&frame.context_at(index).tool_call.arguments_json).unwrap()
}
fn calls(session: &PersistedAgentSession, anchor: &str) -> Vec<ToolCall> {
    let data = session.assistant_data.as_ref().unwrap();
    let logical = |index: usize| {
        native_ids::stable(
            Kind::AssistantReadAuthorization,
            &(
                &session.actor_id,
                &session.conversation_id,
                &data.retrieval_budget.episode_id,
                "window-producer",
                format!("window-call-{index}"),
            ),
        )
    };
    let binding = RetrievalQueryBinding {
        actor_id: session.actor_id.clone(),
        root_conversation_id: session.conversation_id.clone(),
        source_device_ref: data.source_device_ref.clone(),
        retrieval_episode_id: data.retrieval_budget.episode_id.clone(),
        task_id: None,
        scope_sha256: scope_sha256(session).unwrap(),
        history_scope: data.history_access_scope.clone(),
        memory_device_rule: None,
        request_memory_device_refs: Vec::new(),
        destination: super::model_sources::destination("model"),
        authority_expires_at_unix_ms: None,
    };
    let query = FrozenRetrievalQuery::query_id(&binding, &logical(0)).unwrap();
    (0..8).map(|index| ToolCall { id: format!("window-call-{index}"), name: data_tools::READ_CONVERSATION.into(),
        arguments_json: serde_json::json!({"source_id":anchor,"view":"latest","limit":20,"max_bytes":8192,
            "cursor":if index == 0 { None } else { Some(conversation_window::page_cursor(&query, &logical(index-1), false).unwrap().1) }}).to_string() }).collect()
}
async fn capture(frame: &Frame, name: &str, body: &str, recorded: u64) -> String {
    capture_at(
        &frame.store,
        &frame
            .session
            .assistant_data
            .as_ref()
            .unwrap()
            .source_device_ref,
        "past-window",
        name,
        body,
        &[],
        recorded,
    )
    .await
}
async fn search(
    frame: &Frame,
) -> desk_diagnose_core::ai_assistant::seam::AssistantDataRead<
    desk_agent_protocol::ai_assistant::history::ConversationSearchResult,
> {
    let request: ConversationSearchRequest =
        serde_json::from_str(&frame.context().tool_call.arguments_json).unwrap();
    conversation_search::search(&frame.store.db, frame.context(), &request)
        .await
        .unwrap()
}
pub(super) async fn fixture(label: &str, count: usize) -> (Frame, Vec<(String, String)>) {
    let frame = Frame::new(label, true).await;
    let body = "report original anchor".to_owned();
    let anchor = capture(&frame, "original-anchor", &body, frame.now - 1000).await;
    let mut expected = vec![(anchor.clone(), body)];
    for index in 0..count {
        let body = format!("detail {index:02} {}", "汉字\\\"".repeat(80));
        // Native admission order remains stable if the wall clock moves back.
        let id = capture(
            &frame,
            &format!("tail-{index}"),
            &body,
            frame.now - 500 - index as u64,
        )
        .await;
        expected.push((id, body));
    }
    let read = search(&frame).await;
    assert_eq!(read.result.hits.len(), 1);
    assert_eq!(read.result.hits[0].source_id, anchor);
    let frame = frame
        .after_search(read, Some(8192), |session, result| {
            calls(session, &result.hits[0].source_id)
        })
        .await;
    (frame, expected)
}

#[tokio::test]
async fn accepted_search_output_can_read_original_tail_pages_without_skips_duplicates_or_newly_admitted_fragments()
 {
    let (frame, expected) = fixture("window-flow-pages", 23).await;
    let anchor = &expected[0].0;
    let native_grants = authorization::Entity::find()
        .count(&frame.store.db)
        .await
        .unwrap();
    let mut emitted = Vec::new();
    let mut previous = None;
    let mut observed = None;
    let mut ended = false;
    let mut late = None;
    for index in 0..8 {
        let request = request(&frame, index);
        assert_eq!(request.cursor, previous);
        let page = conversation_window::read(&frame.store.db, frame.context_at(index), &request)
            .await
            .unwrap();
        let body = serde_json::to_string(&page.result).unwrap();
        assert_eq!(
            page.result.actual_bytes,
            serialized_tool_response_bytes(&frame.context_at(index).tool_call.id, &body, None)
                .unwrap()
        );
        assert!(page.result.actual_bytes <= 8192);
        assert!(!page.result.messages.is_empty());
        let proof = page.projection.as_ref().unwrap();
        proof.validate_cached_result(&body).unwrap();
        assert_eq!(proof.usage.purpose, SourceReadPurpose::ModelHistory);
        assert_eq!(proof.usage.roots.len(), page.result.messages.len());
        assert_eq!(proof.usage.decision_roots.len(), 1);
        assert_eq!(&proof.usage.decision_roots[0].source_id, anchor);
        assert_eq!(page.result.conversation_id, "past-window");
        if index == 0 {
            assert!(page.result.messages.len() < 20);
            assert!(
                !page
                    .result
                    .messages
                    .iter()
                    .any(|message| &message.source_id == anchor)
            );
            assert!(
                !proof
                    .usage
                    .roots
                    .iter()
                    .any(|root| &root.source_id == anchor)
            );
            assert!(
                proof
                    .usage
                    .history_dependencies
                    .iter()
                    .any(|source| &source.source_id == anchor)
            );
            observed = Some(page.result.source_observed_through_unix_ms);
            let before = quota::Entity::find()
                .one(&frame.store.db)
                .await
                .unwrap()
                .unwrap();
            let replay =
                conversation_window::read(&frame.store.db, frame.context_at(index), &request)
                    .await
                    .unwrap();
            assert_eq!(replay.result, page.result);
            assert_eq!(replay.projection, page.projection);
            assert_eq!(
                quota::Entity::find()
                    .one(&frame.store.db)
                    .await
                    .unwrap()
                    .unwrap(),
                before
            );
            late = Some(
                capture(
                    &frame,
                    "late-tail",
                    "new detail outside original observation",
                    frame.now - 900,
                )
                .await,
            );
        }
        assert_eq!(Some(page.result.source_observed_through_unix_ms), observed);
        let mut ids = Vec::new();
        for message in &page.result.messages {
            let item = expected
                .iter()
                .find(|(id, _)| *id == message.source_id)
                .unwrap();
            assert_eq!(message.content, item.1);
            ids.push(message.source_id.clone());
        }
        assert!(ids.iter().all(|id| !emitted.contains(id)));
        ids.extend(emitted);
        emitted = ids;
        previous = page.result.previous_cursor.clone();
        assert!(page.result.next_cursor.is_none());
        if previous.is_none() {
            ended = true;
            break;
        }
    }
    assert!(ended);
    assert_eq!(
        emitted,
        expected
            .iter()
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>()
    );
    assert_eq!(emitted.iter().collect::<BTreeSet<_>>().len(), emitted.len());
    assert!(!emitted.contains(late.as_ref().unwrap()));
    assert_eq!(
        authorization::Entity::find()
            .count(&frame.store.db)
            .await
            .unwrap(),
        native_grants
    );
    let queries = query_row::Entity::find()
        .all(&frame.store.db)
        .await
        .unwrap();
    let window = queries.into_iter().map(|row| retrieval_cache::decode_row(row).unwrap())
        .find(|query| matches!(query.proof.request, desk_diagnose_core::ai_assistant::retrieval_state::FrozenRetrievalRequest::Window { .. })).unwrap();
    assert_eq!(window.proof.seen_sources.len(), expected.len());
    assert!(window.proof.snapshot.corpus_generation > window.proof.source_observation_generation);
}

#[tokio::test]
async fn deleting_or_disallowing_the_original_anchor_blocks_cached_and_continued_windows_without_fallback()
 {
    for private in [false, true] {
        let (frame, expected) = fixture(&format!("window-anchor-lost-{private}"), 8).await;
        let first_request = request(&frame, 0);
        let first = conversation_window::read(&frame.store.db, frame.context(), &first_request)
            .await
            .unwrap();
        assert!(first.result.previous_cursor.is_some());
        if private {
            private_source(&frame.store, &expected[0].0).await;
        } else {
            history_row::Entity::delete_many()
                .filter(history_row::Column::SourceId.eq(&expected[0].0))
                .exec(&frame.store.db)
                .await
                .unwrap();
        }
        let before = quota::Entity::find()
            .one(&frame.store.db)
            .await
            .unwrap()
            .unwrap();
        assert!(
            conversation_window::read(&frame.store.db, frame.context(), &first_request)
                .await
                .is_err()
        );
        let next_request = request(&frame, 1);
        assert_eq!(next_request.cursor, first.result.previous_cursor);
        assert!(
            conversation_window::read(&frame.store.db, frame.context_at(1), &next_request)
                .await
                .is_err()
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
}

#[tokio::test]
async fn window_query_metadata_failure_rolls_back_the_prepared_response_and_full_charge() {
    let (frame, _) = fixture("window-cache-failure", 2).await;
    let before = quota::Entity::find()
        .one(&frame.store.db)
        .await
        .unwrap()
        .unwrap();
    let query_count = query_row::Entity::find()
        .count(&frame.store.db)
        .await
        .unwrap();
    let projection_count = projection_row::Entity::find()
        .count(&frame.store.db)
        .await
        .unwrap();
    frame.store.db.execute_unprepared("CREATE TRIGGER reject_window_cache BEFORE INSERT ON agent_assistant_retrieval_query BEGIN SELECT RAISE(ABORT, 'window cache failure'); END").await.unwrap();
    assert!(
        conversation_window::read(&frame.store.db, frame.context(), &request(&frame, 0))
            .await
            .is_err()
    );
    assert_eq!(
        query_row::Entity::find()
            .count(&frame.store.db)
            .await
            .unwrap(),
        query_count
    );
    assert_eq!(
        projection_row::Entity::find()
            .count(&frame.store.db)
            .await
            .unwrap(),
        projection_count
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
async fn a_read_request_cannot_replace_the_source_arguments_of_its_accepted_native_call() {
    let (frame, _) = fixture("window-wrong-arguments", 1).await;
    let mut changed = request(&frame, 0);
    changed.source_id = native_ids::stable(Kind::AssistantSource, &"another-source");
    let before = quota::Entity::find()
        .one(&frame.store.db)
        .await
        .unwrap()
        .unwrap();
    assert!(
        conversation_window::read(&frame.store.db, frame.context(), &changed)
            .await
            .is_err()
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
async fn guessing_an_existing_source_id_in_an_output_does_not_make_it_an_observed_input_source() {
    let frame = Frame::with_prepared_calls(
        "window-unobserved",
        true,
        Some(8192),
        async |store, session| {
            let source = capture_at(
                store,
                &session.assistant_data.as_ref().unwrap().source_device_ref,
                "past-window",
                "unobserved-fragment",
                "report retained but absent from this model input",
                &[],
                chrono::Utc::now().timestamp_millis() as u64,
            )
            .await;
            vec![ToolCall {
                id: "unobserved-read".into(),
                name: data_tools::READ_CONVERSATION.into(),
                arguments_json:
                    serde_json::json!({"source_id":source,"view":"latest","max_bytes":8192})
                        .to_string(),
            }]
        },
    )
    .await;
    let request = request(&frame, 0);
    assert!(
        history_row::Entity::find()
            .filter(history_row::Column::SourceId.eq(&request.source_id))
            .one(&frame.store.db)
            .await
            .unwrap()
            .is_some()
    );
    let before = quota::Entity::find()
        .one(&frame.store.db)
        .await
        .unwrap()
        .unwrap();
    let projections = projection_row::Entity::find()
        .count(&frame.store.db)
        .await
        .unwrap();
    assert!(
        conversation_window::read(&frame.store.db, frame.context(), &request)
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
