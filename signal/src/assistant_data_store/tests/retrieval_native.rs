use super::super::retrieval_native as gate;
use super::*;
use crate::entity::{
    agent_assistant_model_output as archived, agent_assistant_origin as origin,
    agent_assistant_read_projection as projection_row, agent_session,
    model_egress_receipt as native,
};
use desk_agent_protocol::data_lineage::{DataEnvelope, DestinationIdentity};
use desk_diagnose_core::{
    ai_assistant::{
        data_tools,
        history::{RetrievalCallKind, search_call_kind},
        read_projection::request_sha256,
        seam::AssistantDataCall,
    },
    chat::{ChatMessage, ModelTurn, StopReason, ToolCall},
    model_egress::ModelEgressPolicy,
    native_ids::{self, Kind},
    session::PersistedAgentSession,
};
use sea_orm::{PaginatorTrait, sea_query::Expr};

pub(super) struct Frame {
    pub(super) store: TestStore,
    pub(super) session: Box<PersistedAgentSession>,
    calls: Vec<ToolCall>,
    arguments: Vec<Option<data_tools::ParsedDataCall>>,
    control_targets: Vec<desk_agent_protocol::ai_assistant::memory::MemoryForgetTarget>,
    logical: Vec<String>,
    parent: DataEnvelope,
    destination: DestinationIdentity,
    original: String,
    pub(super) now: u64,
    producer_id: String,
    model_request: desk_diagnose_core::seam::ModelRequest,
}
impl Frame {
    pub(super) async fn new(label: &str, accepted: bool) -> Self {
        Self::configured(label, accepted, |_| {
            vec![ToolCall {
                id: "history-call".into(),
                name: data_tools::SEARCH_CONVERSATIONS.into(),
                arguments_json: "{\"queries\":[\"report\"]}".into(),
            }]
        })
        .await
    }
    pub(super) async fn configured(
        label: &str,
        accepted: bool,
        make_calls: impl FnOnce(&PersistedAgentSession) -> Vec<ToolCall>,
    ) -> Self {
        Self::with_maximum(label, accepted, None, make_calls).await
    }
    pub(super) async fn with_maximum(
        label: &str,
        accepted: bool,
        maximum: Option<u32>,
        make_calls: impl FnOnce(&PersistedAgentSession) -> Vec<ToolCall>,
    ) -> Self {
        Self::with_prepared_calls(label, accepted, maximum, async |_, session| {
            make_calls(session)
        })
        .await
    }
    pub(super) async fn with_prepared_calls(
        label: &str,
        accepted: bool,
        maximum: Option<u32>,
        make_calls: impl std::ops::AsyncFnOnce(&TestStore, &PersistedAgentSession) -> Vec<ToolCall>,
    ) -> Self {
        Self::with_call_scope(label, accepted, maximum, false, true, None, make_calls).await
    }
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn with_call_scope(
        label: &str,
        accepted: bool,
        maximum: Option<u32>,
        capture_owner_input: bool,
        include_memory: bool,
        model_base_url: Option<&str>,
        make_calls: impl std::ops::AsyncFnOnce(&TestStore, &PersistedAgentSession) -> Vec<ToolCall>,
    ) -> Self {
        Self::with_call_scope_and_inputs(
            label,
            accepted,
            maximum,
            capture_owner_input,
            include_memory,
            model_base_url,
            None,
            make_calls,
        )
        .await
    }
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn with_call_scope_and_inputs(
        label: &str,
        accepted: bool,
        maximum: Option<u32>,
        capture_owner_input: bool,
        include_memory: bool,
        model_base_url: Option<&str>,
        owner_inputs: Option<&[(&str, &str)]>,
        make_calls: impl std::ops::AsyncFnOnce(&TestStore, &PersistedAgentSession) -> Vec<ToolCall>,
    ) -> Self {
        let (store, model_request, proof, original) =
            super::model_output::admitted_with_owner_input(
                label,
                capture_owner_input,
                include_memory,
                model_base_url,
                owner_inputs,
            )
            .await;
        let destination = proof.source_envelope.allowed_destinations[0].clone();
        let row = agent_session::Entity::find()
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        let mut session = Box::new(PersistedAgentSession::decode_json(&row.state_json).unwrap());
        let calls = make_calls(&store, &session).await;
        let control_targets = data_tools::forget_batch(&calls)
            .ok()
            .flatten()
            .unwrap_or_default();
        let arguments = calls
            .iter()
            .map(|call| data_tools::parse(call).ok())
            .collect();
        assert!(!calls.is_empty());
        let parent = ModelEgressPolicy {
            destination: destination.clone(),
            selected_source_tools: Default::default(),
            export_authorization_id: "export".into(),
            now_unix_ms: chrono::Utc::now().timestamp_millis() as u64,
            byte_cap: desk_diagnose_core::sink_authorizer::MAX_SINK_BYTES,
            permission_resume: false,
        }
        .derive_model_output_envelope(
            &ModelTurn {
                tool_calls: calls.clone(),
                stop_reason: StopReason::ToolUse,
                ..Default::default()
            },
            &model_request
                .messages
                .iter()
                .filter_map(|message| message.data_envelope.clone())
                .collect::<Vec<_>>(),
        )
        .unwrap();
        if accepted {
            crate::model_egress_store::SignalModelEgressStore::new(store.db.clone())
                .mark_succeeded("native-output", &parent)
                .await
                .unwrap();
        }
        let now = chrono::Utc::now().timestamp_millis() as u64;
        let mut producer = ChatMessage::assistant_tool_calls(
            "read-producer",
            "",
            calls.iter().map(ToolCall::to_ref).collect(),
        );
        producer.turn_id = session.current_turn_id.clone();
        producer.data_envelope = Some(parent.clone());
        session.conversation.push(producer);
        session.version += 1;
        let data = session.assistant_data.as_mut().unwrap();
        let mut logical = Vec::new();
        for call in &calls {
            let parsed = match data_tools::parse(call) {
                Ok(parsed) => parsed,
                Err(_) => {
                    if call.name
                        != desk_diagnose_core::permission_tools::REQUEST_CAPABILITY_GRANTS_TOOL_NAME
                        && desk_diagnose_core::read_tools::build_read_operation(call).is_err()
                    {
                        assert!(desk_diagnose_core::subagent::tools::effect(&call.name).is_some() || call.name == desk_diagnose_core::command_confirmation::COMMAND_TOOL
                                || desk_diagnose_core::ai_assistant::ai_assistant_provider_registry_ref()
                                    .capability_for_tool(&call.name).is_some_and(|capability| capability.wire.effect.is_side_effecting()));
                    }
                    logical.push(native_ids::stable(
                        Kind::AssistantReadAuthorization,
                        &(&session.conversation_id, "read-producer", &call.id),
                    ));
                    continue;
                }
            };
            if matches!(parsed, data_tools::ParsedDataCall::ManageMemory(_)) {
                logical.push(native_ids::stable(
                    Kind::AssistantReadAuthorization,
                    &(&session.conversation_id, "read-producer", &call.id),
                ));
                continue;
            }
            let id = native_ids::stable(
                Kind::AssistantReadAuthorization,
                &(
                    &session.actor_id,
                    &session.conversation_id,
                    &data.retrieval_budget.episode_id,
                    "read-producer",
                    &call.id,
                ),
            );
            let kind = match parsed {
                data_tools::ParsedDataCall::SearchConversations(request) => {
                    search_call_kind(&request)
                }
                data_tools::ParsedDataCall::ReadConversation(_)
                | data_tools::ParsedDataCall::ReadMemoryHistory(_)
                | data_tools::ParsedDataCall::ReadMemorySource { .. } => RetrievalCallKind::Read,
                _ => RetrievalCallKind::Relevance,
            };
            let normal = if kind == RetrievalCallKind::Relevance {
                16384
            } else {
                32768
            };
            let maximum = maximum.unwrap_or(normal).min(normal);
            data.retrieval_budget
                .reserve(
                    &id,
                    &request_sha256(&call.name, &call.arguments_json).unwrap(),
                    kind,
                    maximum,
                    now,
                )
                .unwrap();
            logical.push(id);
        }
        agent_session::Entity::update_many()
            .col_expr(
                agent_session::Column::StateJson,
                Expr::value(session.encode_json_for_storage().unwrap()),
            )
            .col_expr(agent_session::Column::Version, Expr::value(session.version))
            .filter(agent_session::Column::ConversationId.eq(&session.conversation_id))
            .exec(&store.db)
            .await
            .unwrap();
        Self {
            store,
            session,
            calls,
            arguments,
            control_targets,
            logical,
            parent,
            destination,
            original,
            now,
            producer_id: "read-producer".into(),
            model_request,
        }
    }
    pub(super) async fn change_persisted_arguments(&self, arguments_json: &str) {
        let mut stored = self.session.clone_boxed();
        let producer = stored
            .conversation
            .iter_mut()
            .find(|message| message.message_id == self.producer_id)
            .unwrap();
        producer.tool_calls[0].arguments_json = arguments_json.into();
        agent_session::Entity::update_many()
            .col_expr(
                agent_session::Column::StateJson,
                Expr::value(serde_json::to_string(&stored).unwrap()),
            )
            .filter(agent_session::Column::ConversationId.eq(&self.session.conversation_id))
            .exec(&self.store.db)
            .await
            .unwrap();
    }
    pub(super) fn context(&self) -> AssistantDataCall<'_> {
        self.context_at(0)
    }
    pub(super) fn context_at(&self, index: usize) -> AssistantDataCall<'_> {
        let reservation = self
            .session
            .assistant_data
            .as_ref()
            .unwrap()
            .retrieval_budget
            .reservations
            .iter()
            .find(|reservation| reservation.logical_call_id == self.logical[index]);
        assert!(
            reservation.is_some()
                || matches!(
                    data_tools::parse(&self.calls[index]).unwrap(),
                    data_tools::ParsedDataCall::ManageMemory(_)
                )
        );
        AssistantDataCall {
            session: &self.session,
            tool_call: &self.calls[index],
            arguments: self.arguments[index]
                .as_ref()
                .expect("data tool boundary parse"),
            producer_message: self
                .session
                .conversation
                .iter()
                .find(|message| message.message_id == self.producer_id)
                .unwrap(),
            logical_call_id: &self.logical[index],
            retrieval_reservation: reservation,
            producer_output_envelope: &self.parent,
            maximum_response_bytes: reservation
                .map_or(16384, |reservation| reservation.reserved_bytes),
            destination: &self.destination,
            now_unix_ms: self.now,
        }
    }
}

// These use real original accepted receipts and source grants, without provider
// I/O. The read fence itself grants no right to send a model request or do I/O.
#[tokio::test]
async fn original_accepted_producer_and_its_current_sources_are_required_without_adding_grants_or_quota()
 {
    let frame = Frame::new("read-native-valid", true).await;
    let before = quota::Entity::find()
        .one(&frame.store.db)
        .await
        .unwrap()
        .unwrap();
    let projections = projection_row::Entity::find()
        .count(&frame.store.db)
        .await
        .unwrap();
    let txn = frame.store.db.begin().await.unwrap();
    let held = gate::prepare_on(&txn, frame.context(), frame.now)
        .await
        .unwrap();
    held.finish_on(frame.now + 1).await.unwrap();
    assert_eq!(
        projection_row::Entity::find().count(&txn).await.unwrap(),
        projections
    );
    assert_eq!(
        quota::Entity::find().one(&txn).await.unwrap().unwrap(),
        before
    );
    drop(held);
    txn.commit().await.unwrap();
    let frame = Frame::new("read-native-unaccepted", false).await;
    let txn = frame.store.db.begin().await.unwrap();
    assert!(
        gate::prepare_on(&txn, frame.context(), frame.now)
            .await
            .is_err()
    );
    txn.rollback().await.unwrap();
}

#[tokio::test]
async fn final_original_session_raw_bytes_and_native_source_dependencies_are_reselected() {
    for variant in 0..4 {
        let frame = Frame::new(&format!("read-native-final-{variant}"), true).await;
        let txn = frame.store.db.begin().await.unwrap();
        let held = gate::prepare_on(&txn, frame.context(), frame.now)
            .await
            .unwrap();
        match variant {
            0 => {
                agent_session::Entity::update_many()
                    .col_expr(
                        agent_session::Column::StateJson,
                        Expr::value(format!(
                            "{} ",
                            frame.session.encode_json_for_storage().unwrap()
                        )),
                    )
                    .exec(&txn)
                    .await
                    .unwrap();
            }
            1 => {
                origin::Entity::update_many()
                    .col_expr(
                        origin::Column::DeletedAtMs,
                        Expr::value(Some(frame.now as i64)),
                    )
                    .filter(origin::Column::OriginId.eq(&frame.original))
                    .exec(&txn)
                    .await
                    .unwrap();
            }
            2 => {
                archived::Entity::delete_many().exec(&txn).await.unwrap();
            }
            _ => {
                runtime::Entity::update_many()
                    .col_expr(runtime::Column::ControlGeneration, Expr::value(2_i64))
                    .exec(&txn)
                    .await
                    .unwrap();
            }
        }
        assert!(
            held.finish_on(frame.now + 1).await.is_err(),
            "variant {variant}"
        );
        drop(held);
        txn.rollback().await.unwrap();
    }
}

#[tokio::test]
async fn original_output_metadata_can_survive_native_gc_but_never_an_expired_current_session() {
    let frame = Frame::new("read-native-archive", true).await;
    native::Entity::delete_many()
        .exec(&frame.store.db)
        .await
        .unwrap();
    let txn = frame.store.db.begin().await.unwrap();
    let held = gate::prepare_on(&txn, frame.context(), frame.now)
        .await
        .unwrap();
    held.finish_on(frame.now + 1).await.unwrap();
    assert!(held.finish_on(frame.now + 100_000).await.is_err());
    drop(held);
    txn.rollback().await.unwrap();
}

#[tokio::test]
async fn matching_source_state_does_not_authorize_a_different_persisted_producer() {
    let frame = Frame::new("read-native-wrong-producer", true).await;
    let mut altered = frame.session.as_ref().clone();
    altered
        .conversation
        .iter_mut()
        .find(|message| message.message_id == "read-producer")
        .unwrap()
        .tool_calls[0]
        .arguments_json = "{}".into();
    agent_session::Entity::update_many()
        .col_expr(
            agent_session::Column::StateJson,
            Expr::value(altered.encode_json_for_storage().unwrap()),
        )
        .filter(agent_session::Column::ConversationId.eq(&frame.session.conversation_id))
        .exec(&frame.store.db)
        .await
        .unwrap();
    let txn = frame.store.db.begin().await.unwrap();
    assert!(
        gate::prepare_on(&txn, frame.context(), frame.now)
            .await
            .is_err()
    );
    txn.rollback().await.unwrap();
}

mod next_model;

mod memory_mutation;

#[tokio::test]
async fn native_save_cannot_remove_an_already_persisted_assistant_scope() {
    let mut frame = Frame::new("native-scope-removal", true).await;
    let before = agent_session::Entity::find()
        .one(&frame.store.db)
        .await
        .unwrap()
        .unwrap();
    frame.session.assistant_data = None;
    let store = crate::agent_session_store::SignalAgentSessionStore::new(frame.store.db.clone());
    assert!(
        desk_diagnose_core::seam::SessionSeam::save(&store, &mut frame.session)
            .await
            .is_err()
    );
    assert!(
        crate::agent_subagent_store::save_main_delegation_session(
            &frame.store.db,
            &mut frame.session
        )
        .await
        .is_err()
    );
    assert_eq!(
        agent_session::Entity::find_by_id(before.id)
            .one(&frame.store.db)
            .await
            .unwrap(),
        Some(before)
    );
}

mod snapshots;
