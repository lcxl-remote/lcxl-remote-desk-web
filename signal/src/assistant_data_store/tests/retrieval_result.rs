use super::super::retrieval_result::{self as result_store, RetrievalResultInput};
use super::*;
use crate::config::connection::DatabaseTransaction;
use crate::entity::{
    agent_assistant_data_authorization as authorization, agent_assistant_origin as origin,
    agent_assistant_quota as quota, agent_assistant_read_projection as projection,
    agent_memory as memory, agent_session,
};
use desk_agent_protocol::{
    ai_assistant::{
        history::AssistantHistoryCoverage, memory::MemorySearchResult, source::SourceReadPurpose,
    },
    data_lineage::{DataEnvelope, DestinationIdentity},
};
use desk_diagnose_core::{
    ai_assistant::{
        data_access::AssistantDataToolAccess,
        data_tools::SEARCH_MEMORIES,
        history::RetrievalCallKind,
        read_projection::request_sha256,
        seam::{AssistantDataCall, AssistantDataReadUse},
    },
    chat::{ChatMessage, ModelTurn, StopReason, ToolCall},
    model_egress::ModelEgressPolicy,
    model_message_labels::model_bound_user_message,
    native_ids::{self, Kind},
    session::PersistedAgentSession,
};
use sea_orm::{PaginatorTrait, sea_query::Expr};

// These storage-phase fixtures use actual memory/evidence rows. Native actor,
// task, producer receipt and final raw-session gates remain separate callers.
struct Frame {
    store: TestStore,
    session: Box<PersistedAgentSession>,
    call: ToolCall,
    arguments: desk_diagnose_core::ai_assistant::data_tools::ParsedDataCall,
    logical: String,
    parent: DataEnvelope,
    destination: DestinationIdentity,
    access: AssistantDataToolAccess,
    usage: AssistantDataReadUse,
    result: MemorySearchResult,
    policy: AssistantDataPolicy,
    now: u64,
}
impl Frame {
    async fn new(label: &str) -> Self {
        let (store, _, old, _) = super::model_gate::fixture(label).await;
        let row = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq("root"))
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        let mut session = Box::new(PersistedAgentSession::decode_json(&row.state_json).unwrap());
        let now = chrono::Utc::now().timestamp_millis() as u64;
        let destination = super::model_sources::destination("model");
        let call = ToolCall {
            id: "lookup".into(),
            name: SEARCH_MEMORIES.into(),
            arguments_json: "{\"queries\":[\"preference\"]}".into(),
        };
        let model = ModelEgressPolicy {
            destination: destination.clone(),
            selected_source_tools: Default::default(),
            export_authorization_id: "read-producer".into(),
            now_unix_ms: now,
            byte_cap: desk_diagnose_core::sink_authorizer::MAX_SINK_BYTES,
            permission_resume: false,
        };
        let input = model_bound_user_message(
            "query".into(),
            "find my preferences".into(),
            destination.clone(),
        )
        .unwrap()
        .data_envelope
        .unwrap();
        let turn = ModelTurn {
            tool_calls: vec![call.clone()],
            stop_reason: StopReason::ToolUse,
            ..Default::default()
        };
        let parent = model.derive_model_output_envelope(&turn, &[input]).unwrap();
        let mut producer =
            ChatMessage::assistant_tool_calls("read-producer", "", vec![call.to_ref()]);
        producer.data_envelope = Some(parent.clone());
        session.conversation.push(producer);
        let actor = session.actor_id.clone();
        let root = session.conversation_id.clone();
        let data = session.assistant_data.as_mut().unwrap();
        let logical = native_ids::stable(
            Kind::AssistantReadAuthorization,
            &(
                &actor,
                &root,
                &data.retrieval_budget.episode_id,
                "read-producer",
                &call.id,
            ),
        );
        data.retrieval_budget
            .reserve(
                &logical,
                &request_sha256(&call.name, &call.arguments_json).unwrap(),
                RetrievalCallKind::Relevance,
                16384,
                now,
            )
            .unwrap();
        let access = super::model_sources::access(&store).await;
        let fact = memory::Entity::find()
            .filter(memory::Column::MemoryId.eq(&old.usage.memories[0].memory_id))
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        let record = super::super::memory::owner_record_on(&store.db, "1", &fact, now)
            .await
            .unwrap()
            .unwrap();
        let result = MemorySearchResult {
            memories: vec![record],
            effective_queries: vec![vec!["preference".into()]],
            has_more: false,
            next_cursor: None,
            memory_state_generation: access.memory_state_generation,
            coverage: AssistantHistoryCoverage {
                retained_from_unix_ms: None,
                retained_until_unix_ms: None,
                indexed_through_unix_ms: None,
                pending: 0,
                failed: 0,
                has_gaps: false,
                partial: false,
                reasons: Vec::new(),
            },
        };
        let policy = store.db.config_read().await.assistant_data_policy.clone();
        let arguments = desk_diagnose_core::ai_assistant::data_tools::parse(&call).unwrap();
        Self {
            store,
            session,
            call,
            arguments,
            logical,
            parent,
            destination,
            access,
            usage: old.usage,
            result,
            policy,
            now,
        }
    }
    fn context(&self) -> AssistantDataCall<'_> {
        AssistantDataCall {
            session: &self.session,
            tool_call: &self.call,
            arguments: &self.arguments,
            producer_message: self
                .session
                .conversation
                .iter()
                .find(|message| message.message_id == "read-producer")
                .unwrap(),
            logical_call_id: &self.logical,
            retrieval_reservation: Some(
                &self
                    .session
                    .assistant_data
                    .as_ref()
                    .unwrap()
                    .retrieval_budget
                    .reservations[0],
            ),
            producer_output_envelope: &self.parent,
            maximum_response_bytes: 16384,
            destination: &self.destination,
            now_unix_ms: self.now,
        }
    }
}
async fn begin_actor(store: &TestStore) -> DatabaseTransaction {
    let txn = store.db.begin().await.unwrap();
    ensure_runtime_on(&txn, "1").await.unwrap();
    txn
}
async fn metadata(store: &TestStore) -> i64 {
    quota::Entity::find()
        .filter(quota::Column::ActorId.eq("1"))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap()
        .metadata_bytes
}

#[tokio::test]
async fn actual_source_preparation_is_uncharged_until_final_store_and_replay_keeps_the_original_body()
 {
    let frame = Frame::new("result-replay").await;
    let before = metadata(&frame.store).await;
    let context = frame.context();
    let txn = begin_actor(&frame.store).await;
    let pending = result_store::prepare_on(
        &txn,
        &context,
        &frame.access,
        &frame.policy,
        RetrievalResultInput {
            result: frame.result.clone(),
            purpose: SourceReadPurpose::ModelMemory,
            selected: std::slice::from_ref(&frame.usage),
        },
        frame.now,
    )
    .await
    .unwrap();
    assert_eq!(
        quota::Entity::find()
            .filter(quota::Column::ActorId.eq("1"))
            .one(&txn)
            .await
            .unwrap()
            .unwrap()
            .metadata_bytes,
        before
    );
    let read = result_store::store_on(pending, &frame.policy, frame.now)
        .await
        .unwrap();
    assert_eq!(read.result, frame.result);
    assert_eq!(
        read.projection.as_ref().unwrap().usage.roots,
        frame.usage.roots
    );
    assert_eq!(authorization::Entity::find().count(&txn).await.unwrap(), 0);
    txn.commit().await.unwrap();
    let charged = metadata(&frame.store).await;
    assert!(charged > before);
    let txn = begin_actor(&frame.store).await;
    let replay = result_store::replay_on::<_, MemorySearchResult>(
        &txn,
        &context,
        &frame.access,
        &frame.policy,
        SourceReadPurpose::ModelMemory,
        frame.now + 1,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(replay.result, frame.result);
    assert_eq!(replay.projection, read.projection);
    txn.commit().await.unwrap();
    assert_eq!(metadata(&frame.store).await, charged);
}

#[tokio::test]
async fn source_deletion_in_the_same_transaction_is_reselected_before_any_result_or_quota_commit() {
    let frame = Frame::new("result-reselect").await;
    let before = metadata(&frame.store).await;
    let context = frame.context();
    let initial = projection::Entity::find()
        .count(&frame.store.db)
        .await
        .unwrap();
    let txn = begin_actor(&frame.store).await;
    let pending = result_store::prepare_on(
        &txn,
        &context,
        &frame.access,
        &frame.policy,
        RetrievalResultInput {
            result: frame.result.clone(),
            purpose: SourceReadPurpose::ModelMemory,
            selected: std::slice::from_ref(&frame.usage),
        },
        frame.now,
    )
    .await
    .unwrap();
    origin::Entity::update_many()
        .col_expr(
            origin::Column::DeletedAtMs,
            Expr::value(Some(frame.now as i64)),
        )
        .filter(origin::Column::OriginId.eq(&frame.usage.history_dependencies[0].origin_id))
        .exec(&txn)
        .await
        .unwrap();
    assert!(
        result_store::store_on(pending, &frame.policy, frame.now + 1)
            .await
            .is_err()
    );
    txn.rollback().await.unwrap();
    assert_eq!(metadata(&frame.store).await, before);
    assert_eq!(
        projection::Entity::find()
            .count(&frame.store.db)
            .await
            .unwrap(),
        initial
    );
}

#[tokio::test]
async fn control_changes_and_quota_failures_roll_back_without_a_partial_cached_reply() {
    for control_change in [true, false] {
        let frame = Frame::new(if control_change {
            "result-control"
        } else {
            "result-capacity"
        })
        .await;
        let before = metadata(&frame.store).await;
        let initial = projection::Entity::find()
            .count(&frame.store.db)
            .await
            .unwrap();
        let context = frame.context();
        let txn = begin_actor(&frame.store).await;
        let pending = result_store::prepare_on(
            &txn,
            &context,
            &frame.access,
            &frame.policy,
            RetrievalResultInput {
                result: frame.result.clone(),
                purpose: SourceReadPurpose::ModelMemory,
                selected: std::slice::from_ref(&frame.usage),
            },
            frame.now,
        )
        .await
        .unwrap();
        if control_change {
            runtime::Entity::update_many()
                .col_expr(
                    runtime::Column::ControlGeneration,
                    Expr::value(frame.access.control_generation as i64 + 1),
                )
                .filter(runtime::Column::ActorId.eq("1"))
                .exec(&txn)
                .await
                .unwrap();
        } else {
            txn.execute_unprepared("CREATE TRIGGER reject_result_quota BEFORE UPDATE ON agent_assistant_quota BEGIN SELECT RAISE(ABORT, 'injected quota failure'); END").await.unwrap();
        }
        assert!(
            result_store::store_on(pending, &frame.policy, frame.now + 1)
                .await
                .is_err()
        );
        txn.rollback().await.unwrap();
        assert_eq!(metadata(&frame.store).await, before);
        assert_eq!(
            projection::Entity::find()
                .count(&frame.store.db)
                .await
                .unwrap(),
            initial
        );
    }
}

#[tokio::test]
async fn forgotten_sources_never_replay_even_while_the_original_cached_result_still_exists() {
    let frame = Frame::new("result-forget").await;
    let context = frame.context();
    let txn = begin_actor(&frame.store).await;
    let pending = result_store::prepare_on(
        &txn,
        &context,
        &frame.access,
        &frame.policy,
        RetrievalResultInput {
            result: frame.result.clone(),
            purpose: SourceReadPurpose::ModelMemory,
            selected: std::slice::from_ref(&frame.usage),
        },
        frame.now,
    )
    .await
    .unwrap();
    result_store::store_on(pending, &frame.policy, frame.now)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    frame
        .store
        .forget_memory(
            1,
            &desk_agent_protocol::ai_assistant::memory::MemoryDeleteRequest {
                memory_id: frame.result.memories[0].memory_id.clone(),
                expected_revision: 1,
                operation_id: "forget-read-result".into(),
            },
            &frame.policy,
            frame.now + 1,
        )
        .await
        .unwrap();
    let txn = begin_actor(&frame.store).await;
    assert!(
        result_store::replay_on::<_, MemorySearchResult>(
            &txn,
            &context,
            &frame.access,
            &frame.policy,
            SourceReadPurpose::ModelMemory,
            frame.now + 2
        )
        .await
        .is_err()
    );
    txn.rollback().await.unwrap();
}
