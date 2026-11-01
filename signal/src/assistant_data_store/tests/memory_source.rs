use super::super::memory_source as read;
use super::retrieval_native::Frame;
use super::*;
use crate::entity::{
    agent_assistant_data_authorization as authorization,
    agent_assistant_read_projection as projection_row,
    agent_assistant_retrieval_query as query_row, agent_memory_source as evidence,
};
use desk_agent_protocol::ai_assistant::{memory::MemoryDeleteRequest, source::SourceReadPurpose};
use desk_diagnose_core::{
    ai_assistant::{data_tools, history::serialized_tool_response_bytes},
    chat::ToolCall,
    native_ids::{self, Kind},
};
use sea_orm::PaginatorTrait;

async fn frame(label: &str, accepted: bool, maximum: u32, unknown: bool) -> Frame {
    Frame::with_prepared_calls(label, accepted, None, async |store, session| {
        let memory = &session.assistant_data.as_ref().unwrap().current_memories[0].memory_id;
        let source = evidence::Entity::find()
            .filter(evidence::Column::MemoryId.eq(memory))
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        let source_id = if unknown {
            native_ids::stable(Kind::AssistantSource, &"unobserved-evidence")
        } else {
            source.source_id
        };
        vec![ToolCall {
            id: "memory-source-call".into(),
            name: data_tools::READ_MEMORY_SOURCE.into(),
            arguments_json: serde_json::to_string(
                &serde_json::json!({ "source_id": source_id, "max_bytes": maximum }),
            )
            .unwrap(),
        }]
    })
    .await
}

fn args(frame: &Frame) -> (String, usize) {
    match data_tools::parse(frame.context().tool_call).unwrap() {
        data_tools::ParsedDataCall::ReadMemorySource {
            source_id,
            max_bytes,
        } => (source_id, max_bytes as usize),
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn an_unavailable_evidence_handle_returns_a_model_safe_tool_error() {
    let frame = frame("memory-source-unavailable", true, 32768, true).await;
    let (source_id, _) = args(&frame);
    let host = crate::agent_session_store::SignalAgentSessionStore::new(frame.store.db.clone())
        .with_client_metadata(
            None,
            desk_diagnose_core::session::AgentSessionSurface::AiAssistant,
        );
    let port = desk_diagnose_core::seam::SessionSeam::memory(&host).unwrap();
    let before = projection_row::Entity::find()
        .count(&frame.store.db)
        .await
        .unwrap();
    let error = port
        .read_memory_source(frame.context(), &source_id)
        .await
        .err()
        .expect("unavailable source must be rejected");
    assert!(error.safe_for_model);
    assert_eq!(
        error.kind,
        desk_agent_protocol::AgentErrorKind::InvalidInput
    );
    assert!(error.message.contains("read_conversation"));
    assert_eq!(
        projection_row::Entity::find()
            .count(&frame.store.db)
            .await
            .unwrap(),
        before
    );
}

#[tokio::test]
async fn an_actual_native_producer_can_expand_its_exact_retained_evidence_handle_without_returning_the_whole_fact()
 {
    let frame = frame("memory-source-valid", true, 32768, false).await;
    let (source_id, maximum) = args(&frame);
    let host = crate::agent_session_store::SignalAgentSessionStore::new(frame.store.db.clone())
        .with_client_metadata(
            None,
            desk_diagnose_core::session::AgentSessionSurface::AiAssistant,
        );
    let port = desk_diagnose_core::seam::SessionSeam::memory(&host).unwrap();
    let source = evidence::Entity::find()
        .filter(evidence::Column::SourceId.eq(&source_id))
        .one(&frame.store.db)
        .await
        .unwrap()
        .unwrap();
    let grants = authorization::Entity::find()
        .count(&frame.store.db)
        .await
        .unwrap();
    let first = port
        .read_memory_source(frame.context(), &source_id)
        .await
        .unwrap();
    assert_eq!(first.result.source_id, source.source_id);
    assert_eq!(first.result.revision, source.memory_revision as u64);
    assert_eq!(first.result.content.as_deref(), Some(source.body.as_str()));
    let proof = first.projection.as_ref().unwrap();
    assert_eq!(proof.usage.purpose, SourceReadPurpose::MemoryEvidence);
    assert_eq!(proof.usage.roots.len(), 1);
    assert_eq!(proof.usage.memories.len(), 1);
    assert_eq!(proof.usage.roots[0].source_id, source_id);
    assert_eq!(proof.usage.memories[0].memory_id, source.memory_id);
    let content = serde_json::to_string(&first.result).unwrap();
    proof.validate_cached_result(&content).unwrap();
    assert!(
        serde_json::to_value(&first.result)
            .unwrap()
            .get("memory_id")
            .is_none()
    );
    assert!(
        serialized_tool_response_bytes(&frame.context().tool_call.id, &content, None).unwrap()
            <= maximum as u32
    );
    let charged = quota::Entity::find()
        .one(&frame.store.db)
        .await
        .unwrap()
        .unwrap();
    let replay = port
        .read_memory_source(frame.context(), &source_id)
        .await
        .unwrap();
    assert_eq!(first.result, replay.result);
    assert_eq!(first.projection, replay.projection);
    assert_eq!(
        quota::Entity::find()
            .one(&frame.store.db)
            .await
            .unwrap()
            .unwrap(),
        charged
    );
    assert_eq!(
        authorization::Entity::find()
            .count(&frame.store.db)
            .await
            .unwrap(),
        grants
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
async fn unaccepted_producers_unknown_handles_and_too_small_framed_budgets_create_no_source_response()
 {
    for variant in [0, 1, 3] {
        let frame = frame(
            &format!("memory-source-reject-{variant}"),
            variant != 0,
            if variant == 3 { 128 } else { 32768 },
            variant == 1,
        )
        .await;
        let (source_id, _) = args(&frame);
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
            read::read(&frame.store.db, frame.context(), &source_id)
                .await
                .is_err()
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
}

#[tokio::test]
async fn forgetting_the_fact_prevents_cached_evidence_reads_without_deleting_the_original_owner_chat()
 {
    let frame = frame("memory-source-forgotten", true, 32768, false).await;
    let (source_id, _) = args(&frame);
    read::read(&frame.store.db, frame.context(), &source_id)
        .await
        .unwrap();
    let target = &frame
        .session
        .assistant_data
        .as_ref()
        .unwrap()
        .current_memories[0];
    frame
        .store
        .forget_memory(
            1,
            &MemoryDeleteRequest {
                memory_id: target.memory_id.clone(),
                expected_revision: target.revision,
                operation_id: "forget-source-memory".into(),
            },
            &AssistantDataPolicy::default(),
            frame.now + 1,
        )
        .await
        .unwrap();
    let before = quota::Entity::find()
        .one(&frame.store.db)
        .await
        .unwrap()
        .unwrap();
    assert!(
        read::read(&frame.store.db, frame.context(), &source_id)
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
    // Owner chat survival has its own retained-copy tests. This assertion only
    // covers the actual original native session still present after forgetting.
    assert_eq!(
        crate::entity::agent_session::Entity::find()
            .count(&frame.store.db)
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn final_source_projection_failure_rolls_back_the_cached_body_and_metadata_charge() {
    let frame = frame("memory-source-rollback", true, 32768, false).await;
    let (source_id, _) = args(&frame);
    let before = quota::Entity::find()
        .one(&frame.store.db)
        .await
        .unwrap()
        .unwrap();
    let projections = projection_row::Entity::find()
        .count(&frame.store.db)
        .await
        .unwrap();
    frame.store.db.execute_unprepared("CREATE TRIGGER reject_memory_source_result BEFORE INSERT ON agent_assistant_read_projection BEGIN SELECT RAISE(ABORT, 'memory source result failure'); END").await.unwrap();
    assert!(
        read::read(&frame.store.db, frame.context(), &source_id)
            .await
            .is_err()
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
