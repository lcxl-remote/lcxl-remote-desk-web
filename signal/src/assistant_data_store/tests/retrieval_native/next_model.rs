use super::*;
use desk_diagnose_core::{
    ai_assistant::{
        history::serialized_tool_response_bytes,
        seam::{AssistantDataRead, SelectedAssistantEnvelope},
    },
    model_message_labels::assistant_data_result_envelope,
    sink_authorizer::SinkProjectionAudit,
};

impl Frame {
    /// Retain a real delivered search result, pass it through another actual
    /// native dispatch/acceptance, then expose that output's original calls.
    /// Both selected reads belong to this original input and retrieval episode.
    pub(crate) async fn after_search<T: serde::Serialize>(
        mut self,
        read: AssistantDataRead<T>,
        maximum: Option<u32>,
        make_calls: impl FnOnce(&PersistedAgentSession, &T) -> Vec<ToolCall>,
    ) -> Self {
        assert_eq!(self.calls.len(), 1);
        let projection = read.projection.as_ref().unwrap();
        let content = serde_json::to_string(&read.result).unwrap();
        projection.validate_cached_result(&content).unwrap();
        let delivered = if self
            .session
            .conversation
            .iter()
            .any(|message| message.message_id == "delivered-history-search")
        {
            format!("delivered-history-search-{}", self.producer_id)
        } else {
            "delivered-history-search".into()
        };
        let mut result = ChatMessage::tool_result(delivered, &self.calls[0].id, &content);
        result.tool_ok = Some(true);
        result.data_envelope = assistant_data_result_envelope(
            Some(&self.parent),
            std::slice::from_ref(&projection.snapshot_envelope().unwrap()),
            &self.calls[0].id,
            &content,
            &self.calls[0].name,
            &projection.usage.retrieval_episode_id,
        )
        .unwrap();
        let bytes = serialized_tool_response_bytes(&self.calls[0].id, &content, None).unwrap();
        let data = self.session.assistant_data.as_mut().unwrap();
        data.retrieval_budget
            .settle(&self.logical[0], bytes)
            .unwrap();
        let calls = make_calls(&self.session, &read.result);
        self.after_result(result, calls, maximum).await
    }

    /// Continue through the real native model dispatch and acceptance contract.
    pub(crate) async fn after_result(
        mut self,
        mut result: ChatMessage,
        calls: Vec<ToolCall>,
        maximum: Option<u32>,
    ) -> Self {
        let ordinal: u32 = self
            .session
            .conversation
            .iter()
            .filter(|message| message.role == desk_diagnose_core::chat::ChatRole::Assistant)
            .count()
            .try_into()
            .unwrap();
        let export = if ordinal == 1 {
            "after-search-export".into()
        } else {
            format!("after-search-export-{ordinal}")
        };
        let producer = self
            .session
            .conversation
            .iter()
            .find(|message| message.message_id == self.producer_id)
            .unwrap()
            .clone();
        result.turn_id = producer.turn_id.clone();
        self.session.conversation.push(result.clone());
        let sessions =
            crate::agent_session_store::SignalAgentSessionStore::new(self.store.db.clone());
        desk_diagnose_core::seam::SessionSeam::save(&sessions, &mut self.session)
            .await
            .unwrap();
        self.model_request.messages.extend([producer, result]);
        let selected = self
            .model_request
            .messages
            .iter()
            .filter_map(|message| message.data_envelope.clone())
            .collect::<Vec<_>>();
        let manifest = self.model_request.assistant_data.as_mut().unwrap();
        manifest.session_version = self.session.version;
        manifest.selected_envelopes = selected
            .iter()
            .map(|envelope| SelectedAssistantEnvelope {
                envelope_id: envelope.envelope_id.clone(),
            })
            .collect();
        self.model_request
            .assistant_data
            .as_ref()
            .unwrap()
            .validate_for(&self.session, &self.model_request)
            .unwrap();
        self.save().await;
        assert!(!calls.is_empty());
        let now = chrono::Utc::now().timestamp_millis() as u64;
        let parent = ModelEgressPolicy {
            destination: self.destination.clone(),
            selected_source_tools: Default::default(),
            export_authorization_id: export.clone(),
            now_unix_ms: now,
            byte_cap: desk_diagnose_core::sink_authorizer::MAX_SINK_BYTES,
            permission_resume: false,
        }
        .derive_model_output_envelope(
            &ModelTurn {
                tool_calls: calls.clone(),
                stop_reason: StopReason::ToolUse,
                ..Default::default()
            },
            &selected,
        )
        .unwrap();
        let sink = SinkProjectionAudit {
            destination: self.destination.clone(),
            envelope_ids: selected
                .iter()
                .map(|envelope| envelope.envelope_id.clone())
                .collect(),
            total_bytes: selected
                .iter()
                .map(|envelope| envelope.content.size_bytes() as usize)
                .sum(),
        };
        use crate::model_egress_store::SignalModelEgressStore;
        let native_id = if ordinal == 1 {
            "native-after-search".into()
        } else {
            format!("native-after-search-{ordinal}")
        };
        let txn = self.store.db.begin().await.unwrap();
        let prepared = super::super::super::model_gate::prepare_on(&txn, &self.model_request)
            .await
            .unwrap()
            .unwrap();
        SignalModelEgressStore::record_dispatch_intent_on(
            &txn,
            native_id.clone(),
            export.clone(),
            u64::from(ordinal) + 1,
            &sink,
            &selected,
        )
        .await
        .unwrap();
        SignalModelEgressStore::link_context_dispatch_on(
            &txn,
            &native_id,
            &self.session.conversation_id,
            false,
        )
        .await
        .unwrap();
        super::super::super::model_gate::authorize_on(
            &txn,
            Some(&prepared),
            &self.model_request,
            &native_id,
        )
        .await
        .unwrap();
        txn.commit().await.unwrap();
        let native_store = SignalModelEgressStore::new(self.store.db.clone());
        native_store
            .record_terminal_usage(
                &native_id,
                &desk_diagnose_core::chat::TokenUsage {
                    input_tokens: Some(9),
                    output_tokens: Some(3),
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();
        native_store
            .mark_succeeded(&native_id, &parent)
            .await
            .unwrap();
        // A tool reservation follows actual native acceptance, not the time
        // before preparing and recording that model call.
        let now = chrono::Utc::now().timestamp_millis() as u64;
        self.producer_id = if ordinal == 1 {
            "window-producer".into()
        } else {
            format!("window-producer-{ordinal}")
        };
        let mut producer = ChatMessage::assistant_tool_calls(
            &self.producer_id,
            "",
            calls.iter().map(ToolCall::to_ref).collect(),
        );
        producer.turn_id = self.session.current_turn_id.clone();
        producer.data_envelope = Some(parent.clone());
        self.session.conversation.push(producer);
        self.session.version += 1;
        let data = self.session.assistant_data.as_mut().unwrap();
        let mut logical = Vec::new();
        for call in &calls {
            let id = native_ids::stable(
                Kind::AssistantReadAuthorization,
                &(
                    &self.session.actor_id,
                    &self.session.conversation_id,
                    &data.retrieval_budget.episode_id,
                    &self.producer_id,
                    &call.id,
                ),
            );
            let parsed = match data_tools::parse(call) {
                Ok(parsed) => parsed,
                Err(_) => {
                    // Device reads do not reserve the assistant history budget.
                    desk_diagnose_core::read_tools::build_read_operation(call).unwrap();
                    continue;
                }
            };
            if matches!(
                parsed,
                desk_diagnose_core::ai_assistant::data_tools::ParsedDataCall::ManageMemory(_)
            ) {
                logical.push(id);
                continue;
            }
            let kind = match parsed {
                data_tools::ParsedDataCall::SearchConversations(request) => {
                    search_call_kind(&request)
                }
                data_tools::ParsedDataCall::ReadConversation(_)
                | data_tools::ParsedDataCall::ReadMemoryHistory(_)
                | data_tools::ParsedDataCall::ReadMemorySource { .. } => RetrievalCallKind::Read,
                data_tools::ParsedDataCall::SearchMemories(_) => RetrievalCallKind::Relevance,
                _ => panic!("unexpected followup retrieval call"),
            };
            let normal = if kind == RetrievalCallKind::Relevance {
                16384
            } else {
                32768
            };
            data.retrieval_budget
                .reserve(
                    &id,
                    &request_sha256(&call.name, &call.arguments_json).unwrap(),
                    kind,
                    maximum.unwrap_or(normal).min(normal),
                    now,
                )
                .unwrap();
            logical.push(id);
        }
        self.arguments = calls
            .iter()
            .map(|call| data_tools::parse(call).ok())
            .collect();
        self.control_targets = data_tools::forget_batch(&calls)
            .ok()
            .flatten()
            .unwrap_or_default();
        self.calls = calls;
        self.logical = logical;
        self.parent = parent;
        self.now = now;
        self.save().await;
        self
    }

    async fn save(&self) {
        let changed = agent_session::Entity::update_many()
            .col_expr(
                agent_session::Column::StateJson,
                Expr::value(self.session.encode_json_for_storage().unwrap()),
            )
            .col_expr(
                agent_session::Column::Version,
                Expr::value(self.session.version),
            )
            .filter(agent_session::Column::ConversationId.eq(&self.session.conversation_id))
            .exec(&self.store.db)
            .await
            .unwrap();
        assert_eq!(changed.rows_affected, 1);
    }
}
