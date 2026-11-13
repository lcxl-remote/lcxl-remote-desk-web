use super::retrieval_native::Frame;
use super::*;
use crate::agent_session_store::SignalAgentSessionStore as Sessions;
use crate::entity::{
    agent_assistant_data_authorization as authorization, agent_assistant_quota as quota,
    agent_assistant_read_projection as projection, agent_assistant_retrieval_query as query,
    agent_session,
};
use desk_agent_protocol::ai_assistant::{
    history::{ConversationReadRequest, ConversationSearchRequest},
    user_instructions::AssistantDataSettingsUpdate,
};
#[cfg(test)]
use desk_assistant_data_store::entity_table::EntityTableExt;
use desk_diagnose_core::{
    ai_assistant::{data_tools, history::serialized_tool_response_bytes, seam::AssistantDataCall},
    seam::SessionSeam,
    session::{AgentSessionSurface, TriggerOrigin},
};
use sea_orm::PaginatorTrait;
use std::collections::BTreeSet;

fn session_store(frame: &Frame) -> Sessions {
    Sessions::new(frame.store.db.clone())
        .with_client_metadata(None, AgentSessionSurface::AiAssistant)
}
fn search_request(frame: &Frame) -> ConversationSearchRequest {
    serde_json::from_str(&frame.context().tool_call.arguments_json).unwrap()
}

#[tokio::test]
async fn owner_deletion_after_a_real_tool_result_blocks_cached_history_reuse() {
    use crate::entity::agent_file_recovery_cleanup as removal;
    let frame = Frame::new("host-search-delete-source", true).await;
    let device = &frame
        .session
        .assistant_data
        .as_ref()
        .unwrap()
        .source_device_ref;
    let source = super::keyword_search::capture_at(
        &frame.store,
        device,
        "removed-history-root",
        "port-removed",
        "report from a deleted original conversation",
        &[],
        frame.now - 10,
    )
    .await;
    let sessions = session_store(&frame);
    let read = SessionSeam::conversation_search(&sessions)
        .unwrap()
        .search_conversations(frame.context(), &search_request(&frame))
        .await
        .unwrap();
    assert!(read.result.hits.iter().any(|hit| hit.source_id == source));
    removal::ActiveModel {
        conversation_id: Set("removed-history-root".into()),
        actor_id: Set(frame.session.actor_id.clone()),
        device_id: Set(frame.session.device_id.clone()),
        created_at_unix_ms: Set(frame.now as i64),
        history_deleted_at_unix_ms: Set(None),
        next_attempt_at_unix_ms: Set(frame.now as i64),
        attempts: Set(0),
        completed_at_unix_ms: Set(Some(frame.now as i64)),
        ..Default::default()
    }
    .insert(&frame.store.db)
    .await
    .unwrap();
    let schema = sea_orm::Schema::new(frame.store.db.get_database_backend());
    for mut table in [
        schema.entity_table(crate::entity::agent_approval_trace::Entity),
        schema.entity_table(crate::entity::agent_approval_trace_part::Entity),
    ] {
        frame
            .store
            .db
            .execute(&table.if_not_exists().to_owned())
            .await
            .unwrap();
    }
    assert!(
        sessions
            .delete_for_subject(
                "removed-history-root",
                &frame.session.actor_id,
                &frame.session.device_id
            )
            .await
            .unwrap()
            .is_none()
    );
    let before = state(&frame).await;
    assert!(
        SessionSeam::conversation_search(&sessions)
            .unwrap()
            .search_conversations(frame.context(), &search_request(&frame))
            .await
            .is_err()
    );
    assert_eq!(state(&frame).await, before);
    assert_eq!(
        frame
            .store
            .owner_source(1, &source, &AssistantDataPolicy::default(), frame.now + 1)
            .await
            .unwrap()
            .availability,
        desk_agent_protocol::ai_assistant::source::SourceAvailability::Unavailable
    );
}

#[derive(Debug, PartialEq)]
struct State {
    quota: quota::Model,
    queries: u64,
    projections: u64,
    grants: u64,
    jobs: u64,
    facts: u64,
}
async fn state(frame: &Frame) -> State {
    State {
        quota: quota::Entity::find()
            .filter(quota::Column::ActorId.eq(&frame.session.actor_id))
            .one(&frame.store.db)
            .await
            .unwrap()
            .unwrap(),
        queries: query::Entity::find().count(&frame.store.db).await.unwrap(),
        projections: projection::Entity::find()
            .count(&frame.store.db)
            .await
            .unwrap(),
        grants: authorization::Entity::find()
            .count(&frame.store.db)
            .await
            .unwrap(),
        jobs: crate::entity::agent_memory_commit::Entity::find()
            .count(&frame.store.db)
            .await
            .unwrap(),
        facts: crate::entity::agent_memory::Entity::find()
            .count(&frame.store.db)
            .await
            .unwrap(),
    }
}

#[tokio::test]
async fn actual_personal_session_port_searches_native_history_and_replays_the_same_projection() {
    let frame = Frame::new("host-search-port", true).await;
    let device = &frame
        .session
        .assistant_data
        .as_ref()
        .unwrap()
        .source_device_ref;
    let a = super::keyword_search::capture_at(
        &frame.store,
        device,
        "past-A",
        "port-A",
        "report 邮件 A",
        &[],
        frame.now - 20,
    )
    .await;
    let b = super::keyword_search::capture_at(
        &frame.store,
        device,
        "past-B",
        "port-B",
        "report 文件 B",
        &[],
        frame.now - 10,
    )
    .await;
    let sessions = session_store(&frame);
    let port = SessionSeam::conversation_search(&sessions).unwrap();
    let before = state(&frame).await;
    let read = port
        .search_conversations(frame.context(), &search_request(&frame))
        .await
        .unwrap();
    assert_eq!(
        read.result
            .hits
            .iter()
            .map(|hit| hit.source_id.clone())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([a, b])
    );
    let proof = read.projection.as_ref().unwrap();
    let body = serde_json::to_string(&read.result).unwrap();
    proof.validate_cached_result(&body).unwrap();
    assert!(
        serialized_tool_response_bytes(&frame.context().tool_call.id, &body, None).unwrap()
            <= frame.context().maximum_response_bytes
    );
    let stored = state(&frame).await;
    assert_eq!(stored.grants, before.grants);
    assert_eq!(stored.queries, before.queries + 1);
    assert_eq!(stored.projections, before.projections + 1);
    let replay = port
        .search_conversations(frame.context(), &search_request(&frame))
        .await
        .unwrap();
    assert_eq!(serde_json::to_string(&replay.result).unwrap(), body);
    assert_eq!(replay.projection.as_ref().unwrap(), proof);
    assert_eq!(state(&frame).await, stored);
}

#[tokio::test]
async fn actual_personal_session_port_reads_the_observed_original_window() {
    let (frame, expected) = super::conversation_window::fixture("host-read-port", 3).await;
    let sessions = session_store(&frame);
    let port = SessionSeam::conversation_search(&sessions).unwrap();
    let request: ConversationReadRequest =
        serde_json::from_str(&frame.context().tool_call.arguments_json).unwrap();
    let before = state(&frame).await;
    let read = port
        .read_conversation(frame.context(), &request)
        .await
        .unwrap();
    assert_eq!(
        read.result
            .messages
            .iter()
            .map(|message| (message.source_id.clone(), message.content.clone()))
            .collect::<Vec<_>>(),
        expected
    );
    assert!(!read.result.has_more);
    let body = serde_json::to_string(&read.result).unwrap();
    read.projection
        .as_ref()
        .unwrap()
        .validate_cached_result(&body)
        .unwrap();
    assert!(
        serialized_tool_response_bytes(&frame.context().tool_call.id, &body, None).unwrap()
            <= frame.context().maximum_response_bytes
    );
    assert_eq!(state(&frame).await.grants, before.grants);
}

#[tokio::test]
async fn unknown_and_terminal_host_entries_do_not_expose_history_ports() {
    let frame = Frame::new("host-search-entry-scope", true).await;
    let before = state(&frame).await;
    for surface in [
        AgentSessionSurface::Unknown,
        AgentSessionSurface::TerminalAiAssistant,
    ] {
        let sessions = Sessions::new(frame.store.db.clone()).with_client_metadata(None, surface);
        assert!(SessionSeam::conversation_search(&sessions).is_none());
    }
    assert_eq!(state(&frame).await, before);
}

#[tokio::test]
async fn temporary_non_assistant_and_delegated_contexts_cannot_borrow_a_personal_host_port() {
    let frame = Frame::new("host-search-context-scope", true).await;
    let sessions = session_store(&frame);
    let port = SessionSeam::conversation_search(&sessions).unwrap();
    let request = search_request(&frame);
    let before = state(&frame).await;
    for variant in 0..3 {
        let mut changed = (*frame.session).clone();
        match variant {
            0 => changed.assistant_data.as_mut().unwrap().temporary = true,
            1 => changed.surface = AgentSessionSurface::TerminalAiAssistant,
            _ => changed.trigger_origin = TriggerOrigin::DelegatedTask,
        }
        let context = AssistantDataCall {
            session: &changed,
            ..frame.context()
        };
        assert!(port.search_conversations(context, &request).await.is_err());
    }
    assert_eq!(state(&frame).await, before);
}

#[tokio::test]
async fn an_unaccepted_native_producer_or_changed_original_arguments_cannot_enter_a_query() {
    let frame = Frame::new("host-search-unaccepted", false).await;
    let sessions = session_store(&frame);
    let before = state(&frame).await;
    assert!(
        SessionSeam::conversation_search(&sessions)
            .unwrap()
            .search_conversations(frame.context(), &search_request(&frame))
            .await
            .is_err()
    );
    assert_eq!(state(&frame).await, before);
    let frame = Frame::new("host-search-changed-arguments", true).await;
    let sessions = session_store(&frame);
    let request = search_request(&frame);
    frame
        .change_persisted_arguments("{\"queries\":[\"changed persisted request\"]}")
        .await;
    let before = state(&frame).await;
    assert!(
        SessionSeam::conversation_search(&sessions)
            .unwrap()
            .search_conversations(frame.context(), &request)
            .await
            .is_err()
    );
    assert_eq!(state(&frame).await, before);
}

#[tokio::test]
async fn the_current_history_switch_is_checked_even_when_the_host_supports_a_port() {
    let frame = Frame::new("host-search-disabled", true).await;
    let policy = AssistantDataPolicy::default();
    let current = frame.store.settings(1, &policy).await.unwrap();
    let access = super::model_sources::access(&frame.store).await;
    let tools = data_tools::for_session(
        &frame.session,
        &access.settings,
        false,
        false,
        true,
        frame.now,
    );
    assert!(
        tools
            .iter()
            .any(|tool| tool.name() == data_tools::SEARCH_CONVERSATIONS)
    );
    frame
        .store
        .update_settings(
            1,
            &AssistantDataSettingsUpdate {
                expected_revision: current.revision,
                automatic_memory_enabled: current.automatic_memory_enabled,
                model_history_search_enabled: false,

                explanation_version: current.explanation_version,
            },
            &policy,
        )
        .await
        .unwrap();
    let sessions = session_store(&frame);
    let before = state(&frame).await;
    assert!(
        SessionSeam::conversation_search(&sessions)
            .unwrap()
            .search_conversations(frame.context(), &search_request(&frame))
            .await
            .is_err()
    );
    assert_eq!(state(&frame).await, before);
    let access = super::model_sources::access(&frame.store).await;
    let tools = data_tools::for_session(
        &frame.session,
        &access.settings,
        false,
        false,
        true,
        frame.now,
    );
    assert!(
        !tools
            .iter()
            .any(|tool| tool.name() == data_tools::SEARCH_CONVERSATIONS
                || tool.name() == data_tools::READ_CONVERSATION)
    );
}

#[tokio::test]
async fn memory_mutation_host_and_context_gates_reject_without_durable_side_effects() {
    use desk_agent_protocol::ai_assistant::memory::MemoryWriteRequest;
    use desk_diagnose_core::ai_assistant::seam::MemoryMutationSeam;
    let frame = Frame::new("host-memory-mutation-scope", true).await;
    let request: MemoryWriteRequest = serde_json::from_value(serde_json::json!({
        "operation":"remember", "content":"Prefer concise responses",
        "source_refs":[{"kind":"input","id":"unobserved-source","role":"evidence"}]
    }))
    .unwrap();
    let before = state(&frame).await;
    for surface in [
        AgentSessionSurface::Unknown,
        AgentSessionSurface::TerminalAiAssistant,
    ] {
        let host = Sessions::new(frame.store.db.clone()).with_client_metadata(None, surface);
        assert!(SessionSeam::memory_mutation(&host).is_none());
        assert!(
            MemoryMutationSeam::manage_memory(&host, frame.context(), &request)
                .await
                .is_err()
        );
    }

    let host = session_store(&frame);
    let port = SessionSeam::memory_mutation(&host).unwrap();
    for variant in 0..3 {
        let mut changed = (*frame.session).clone();
        match variant {
            0 => changed.assistant_data.as_mut().unwrap().temporary = true,
            1 => changed.surface = AgentSessionSurface::TerminalAiAssistant,
            _ => changed.trigger_origin = TriggerOrigin::DelegatedTask,
        }
        assert!(
            port.manage_memory(
                AssistantDataCall {
                    session: &changed,
                    ..frame.context()
                },
                &request
            )
            .await
            .is_err()
        );
    }
    assert_eq!(state(&frame).await, before);
}

#[tokio::test]
async fn native_callbacks_refresh_context_without_acquiring_memory_or_history_tools() {
    use desk_agent_protocol::ai_assistant::memory::MemoryWriteRequest;
    use desk_diagnose_core::session::WorkKind;
    for origin in [
        TriggerOrigin::WorkCompletion {
            kind: WorkKind::ComputerAction,
        },
        TriggerOrigin::WorkCompletion {
            kind: WorkKind::AgentExec,
        },
    ] {
        let mut frame = Frame::new("callback-context", true).await;
        frame.session.trigger_origin = origin;
        agent_session::Entity::update_many()
            .col_expr(
                agent_session::Column::StateJson,
                sea_orm::sea_query::Expr::value(frame.session.encode_json_for_storage().unwrap()),
            )
            .filter(agent_session::Column::ConversationId.eq(&frame.session.conversation_id))
            .exec(&frame.store.db)
            .await
            .unwrap();
        let host = session_store(&frame);
        let before = state(&frame).await;
        let access = SessionSeam::assistant_data_access(&host, &frame.session, frame.now)
            .await
            .unwrap()
            .expect("native callbacks retain context verification");
        assert!(
            data_tools::for_session(
                &frame.session,
                &access.settings,
                true,
                true,
                true,
                frame.now
            )
            .is_empty()
        );
        assert!(
            SessionSeam::conversation_search(&host)
                .unwrap()
                .search_conversations(frame.context(), &search_request(&frame))
                .await
                .is_err()
        );
        let request: MemoryWriteRequest = serde_json::from_value(serde_json::json!({
            "operation":"remember", "content":"Must remain unauthorized",
            "source_refs":[{"kind":"input", "id":"unobserved", "role":"evidence"}]
        }))
        .unwrap();
        assert!(
            SessionSeam::memory_mutation(&host)
                .unwrap()
                .manage_memory(frame.context(), &request)
                .await
                .is_err()
        );
        let mut stale = frame.session.clone();
        stale.version += 1;
        assert!(
            SessionSeam::assistant_data_access(&host, &stale, frame.now)
                .await
                .is_err()
        );
        assert_eq!(state(&frame).await, before);
    }
}

#[tokio::test]
async fn explicit_child_wait_keeps_the_original_history_scope_and_budget() {
    let mut frame = Frame::new("child-wait-original-scope", true).await;
    let before = frame.session.assistant_data.clone().unwrap();
    let source = super::keyword_search::capture_at(
        &frame.store,
        &before.source_device_ref,
        "earlier-report",
        "wait-source",
        "report from earlier work",
        &[],
        frame.now - 10,
    )
    .await;
    frame.session.trigger_origin = TriggerOrigin::SubAgentCompletion;
    agent_session::Entity::update_many()
        .col_expr(
            agent_session::Column::StateJson,
            sea_orm::sea_query::Expr::value(frame.session.encode_json_for_storage().unwrap()),
        )
        .filter(agent_session::Column::ConversationId.eq(&frame.session.conversation_id))
        .exec(&frame.store.db)
        .await
        .unwrap();
    let host = session_store(&frame);
    let access = SessionSeam::assistant_data_access(&host, &frame.session, frame.now)
        .await
        .unwrap()
        .unwrap();
    assert!(
        !data_tools::for_session(
            &frame.session,
            &access.settings,
            true,
            true,
            true,
            frame.now
        )
        .is_empty()
    );
    let found = SessionSeam::conversation_search(&host)
        .unwrap()
        .search_conversations(frame.context(), &search_request(&frame))
        .await
        .unwrap();
    assert!(found.result.hits.iter().any(|hit| hit.source_id == source));
    let after = frame.session.assistant_data.as_ref().unwrap();
    assert_eq!(after.history_access_scope, before.history_access_scope);
    assert_eq!(after.memory_access_scope, before.memory_access_scope);
    assert_eq!(after.retrieval_budget, before.retrieval_budget);
}
