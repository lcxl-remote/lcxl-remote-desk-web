use super::super::{model_gate, native_export};
use super::*;
use crate::{
    entity::{
        agent_assistant_data_authorization as authorization, agent_assistant_origin as origin,
        agent_assistant_source_use as source_use, agent_session, model_egress_receipt as native,
    },
    model_egress_store::SignalModelEgressStore,
};
use desk_agent_protocol::ai_assistant::memory::MemoryDeleteRequest;
use desk_diagnose_core::{
    ai_assistant::{native_export::AssistantNativeSourceSet, read_projection::ReadProjectionProof},
    chat::{ModelTurn, StopReason, TokenUsage},
    model_egress::ModelEgressPolicy,
    schedule::model_admission::ModelAdmissionError,
    seam::ModelRequest,
};
use sea_orm::sea_query::Expr;

mod owner_history;
mod source_free;

pub(super) fn output(
    proof: &ReadProjectionProof,
) -> desk_agent_protocol::data_lineage::DataEnvelope {
    output_text(proof, "Completed using the selected memory")
}

pub(super) fn output_text(
    proof: &ReadProjectionProof,
    text: &str,
) -> desk_agent_protocol::data_lineage::DataEnvelope {
    ModelEgressPolicy {
        destination: proof.source_envelope.allowed_destinations[0].clone(),
        selected_source_tools: Default::default(),
        export_authorization_id: "export".into(),
        now_unix_ms: chrono::Utc::now().timestamp_millis() as u64,
        byte_cap: desk_diagnose_core::sink_authorizer::MAX_SINK_BYTES,
        permission_resume: false,
    }
    .derive_model_output_envelope(
        &ModelTurn {
            text: text.into(),
            stop_reason: StopReason::EndTurn,
            ..Default::default()
        },
        std::slice::from_ref(&proof.source_envelope),
    )
    .unwrap()
}

pub(super) async fn admitted(label: &str) -> (TestStore, ModelRequest, ReadProjectionProof, i64) {
    admitted_with_owner_input(label, false, true, None, None).await
}

pub(super) async fn admitted_with_owner_input(
    label: &str,
    capture_owner_input: bool,
    include_memory: bool,
    model_base_url: Option<&str>,
    owner_inputs: Option<&[(&str, &str)]>,
) -> (TestStore, ModelRequest, ReadProjectionProof, i64) {
    let (store, mut request, proof, original) =
        super::model_gate::fixture_with_model(label, model_base_url).await;
    if capture_owner_input {
        use desk_diagnose_core::{
            model_message_labels::model_bound_user_message, seam::SessionSeam,
            session::PersistedAgentSession,
        };
        let row = agent_session::Entity::find()
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        let mut session = Box::new(PersistedAgentSession::decode_json(&row.state_json).unwrap());
        let mut inputs = Vec::new();
        if !include_memory {
            inputs.push(
                model_bound_user_message(
                    "interactive-background-input".into(),
                    "我正在维护一个社区项目。".into(),
                    proof.source_envelope.allowed_destinations[0].clone(),
                )
                .unwrap(),
            );
        }
        inputs.push(
            model_bound_user_message(
                "interactive-owner-input".into(),
                "以后请用中文回答，并保持简明。".into(),
                proof.source_envelope.allowed_destinations[0].clone(),
            )
            .unwrap(),
        );
        if let Some(selected) = owner_inputs {
            inputs = selected
                .iter()
                .map(|(id, text)| {
                    model_bound_user_message(
                        (*id).into(),
                        (*text).into(),
                        proof.source_envelope.allowed_destinations[0].clone(),
                    )
                    .unwrap()
                })
                .collect();
        }
        let mut original_row = row;
        for (index, input) in inputs.iter().enumerate() {
            let txn = store.db.begin().await.unwrap();
            if index > 0 {
                session.input_revision += 1;
                session.latest_input_seq += 1;
                let next = session.latest_input_seq;
                session.begin_focus_epoch(next, Vec::new()).unwrap();
                let now = chrono::Utc::now().timestamp_millis() as u64;
                super::super::accept_input_on(&txn, &mut session, "UTC", None, now)
                    .await
                    .unwrap();
            }
            session.conversation.push(input.clone());
            session.version += 1;
            let changed = agent_session::Entity::update_many()
                .col_expr(agent_session::Column::Version, Expr::value(session.version))
                .col_expr(
                    agent_session::Column::StateJson,
                    Expr::value(session.encode_json_for_storage().unwrap()),
                )
                .filter(agent_session::Column::Id.eq(original_row.id))
                .filter(agent_session::Column::Version.eq(original_row.version))
                .filter(agent_session::Column::LeaseToken.eq(original_row.lease_token))
                .filter(agent_session::Column::StateJson.eq(&original_row.state_json))
                .exec(&txn)
                .await
                .unwrap();
            assert_eq!(changed.rows_affected, 1);
            super::super::history::capture_user_input_on(
                &txn,
                &session,
                input,
                session
                    .assistant_data
                    .as_ref()
                    .unwrap()
                    .current_input_recorded_at_unix_ms,
            )
            .await
            .unwrap();
            txn.commit().await.unwrap();
            if index + 1 < inputs.len() {
                original_row = agent_session::Entity::find_by_id(original_row.id)
                    .one(&store.db)
                    .await
                    .unwrap()
                    .unwrap();
            }
        }
        if owner_inputs.is_some() && include_memory {
            // A new focus epoch needs a fresh current-memory projection, just
            // like a real second user turn; the first input's grant is obsolete.
            let block = super::super::prepare_current_memory(
                &store.db,
                &session,
                &proof.source_envelope.allowed_destinations[0],
                "follow-up-current",
                8192,
            )
            .await
            .unwrap();
            assert!(!block.records.is_empty());
            let content =
                desk_diagnose_core::ai_assistant::current_memory::CurrentMemoryPayload::new(
                    block.memory_state_generation,
                    block.records,
                )
                .encode(8192, chrono::Utc::now().timestamp_millis() as u64)
                .unwrap()
                .unwrap();
            let data = session.assistant_data.as_mut().unwrap();
            data.current_memories = block
                .uses
                .iter()
                .flat_map(|usage| usage.memories.clone())
                .collect();
            data.read_uses = block.uses.iter().map(|usage| usage.reference()).collect();
            data.history_dependencies = block
                .uses
                .iter()
                .flat_map(|usage| usage.history_dependencies.clone())
                .collect();
            data.memory_state_generation = block.memory_state_generation;
            session.version += 1;
            agent_session::Entity::update_many()
                .filter(agent_session::Column::ConversationId.eq(&session.conversation_id))
                .col_expr(agent_session::Column::Version, Expr::value(session.version))
                .col_expr(
                    agent_session::Column::StateJson,
                    Expr::value(session.encode_json_for_storage().unwrap()),
                )
                .exec(&store.db)
                .await
                .unwrap();
            let mut message = desk_diagnose_core::chat::ChatMessage::text(
                "follow-up-current",
                desk_diagnose_core::chat::ChatRole::User,
                content,
            );
            message.data_envelope = block.source_envelopes.into_iter().next();
            request.messages = vec![message];
        }
        if model_base_url.is_some() {
            request.delegation_call =
                Some(super::model_provider::reserve_main_budget(&store.db, &mut session).await);
        }
        if !include_memory {
            request.messages.clear();
        }
        request.messages.extend(inputs);
        request.assistant_data = None;
        let host = crate::agent_session_store::SignalAgentSessionStore::new(store.db.clone());
        request.assistant_data = host
            .assistant_data_request_manifest(
                &session,
                &request,
                chrono::Utc::now().timestamp_millis() as u64,
            )
            .await
            .unwrap()
            .map(Box::new);
    }

    let txn = store.db.begin().await.unwrap();
    let prepared = model_gate::prepare_on(&txn, &request)
        .await
        .unwrap()
        .unwrap();
    super::model_gate::native_on(&txn, &request, &proof, "native-output").await;
    model_gate::authorize_on(&txn, Some(&prepared), &request, "native-output")
        .await
        .unwrap();
    txn.commit().await.unwrap();
    SignalModelEgressStore::new(store.db.clone())
        .record_terminal_usage(
            "native-output",
            &TokenUsage {
                input_tokens: Some(12),
                output_tokens: Some(4),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
    (store, request, proof, original)
}

#[tokio::test]
async fn source_aware_completion_accepts_valid_output_with_its_original_usage_and_source_set() {
    let (store, _, proof, _) = admitted("output-valid").await;
    let candidate = output(&proof);
    SignalModelEgressStore::new(store.db.clone())
        .mark_succeeded("native-output", &candidate)
        .await
        .unwrap();
    let row = native::Entity::find()
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.state, crate::model_egress_store::STATE_SUCCEEDED);
    assert_eq!(
        row.model_output_envelope_id.as_deref(),
        Some(candidate.envelope_id.as_str())
    );
    let usage: TokenUsage = serde_json::from_str(row.usage_json.as_deref().unwrap()).unwrap();
    assert_eq!(
        (usage.input_tokens, usage.output_tokens),
        (Some(12), Some(4))
    );
    let set = AssistantNativeSourceSet::decode(row.assistant_source_set_json.as_deref().unwrap())
        .unwrap();
    assert_eq!(
        native_export::load_call_set_on(&store.db, &set)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn a_forgotten_source_blocks_success_without_discarding_actual_token_usage() {
    let (store, _, proof, _) = admitted("output-forgotten").await;
    store
        .forget_memory(
            1,
            &MemoryDeleteRequest {
                memory_id: proof.usage.memories[0].memory_id.clone(),
                expected_revision: 1,
                operation_id: "forget-inflight-output".into(),
            },
            &AssistantDataPolicy::default(),
            chrono::Utc::now().timestamp_millis() as u64,
        )
        .await
        .unwrap();
    let native_store = SignalModelEgressStore::new(store.db.clone());
    assert!(matches!(
        native_store
            .mark_succeeded("native-output", &output(&proof))
            .await,
        Err(ModelAdmissionError::AssistantDataChanged)
    ));
    native_store.mark_failed("native-output").await.unwrap();
    let row = native::Entity::find()
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.state, crate::model_egress_store::STATE_FAILED);
    assert!(row.model_output_envelope_id.is_none() && row.model_output_digest_sha256.is_none());
    let usage: TokenUsage = serde_json::from_str(row.usage_json.as_deref().unwrap()).unwrap();
    assert_eq!(
        (usage.input_tokens, usage.output_tokens),
        (Some(12), Some(4))
    );
}

#[tokio::test]
async fn completed_output_replay_keeps_the_original_receipt_after_forgetting_and_session_cleanup() {
    let (store, _, proof, _) = admitted("output-replay-forgotten").await;
    let output = output(&proof);
    let native_store = SignalModelEgressStore::new(store.db.clone());
    native_store
        .mark_succeeded("native-output", &output)
        .await
        .unwrap();
    store
        .forget_memory(
            1,
            &MemoryDeleteRequest {
                memory_id: proof.usage.memories[0].memory_id.clone(),
                expected_revision: 1,
                operation_id: "forget-after-output".into(),
            },
            &AssistantDataPolicy::default(),
            chrono::Utc::now().timestamp_millis() as u64,
        )
        .await
        .unwrap();
    agent_session::Entity::delete_many()
        .exec(&store.db)
        .await
        .unwrap();
    let original = native::Entity::find().one(&store.db).await.unwrap();
    let charged = quota::Entity::find().one(&store.db).await.unwrap();
    native_store
        .mark_succeeded("native-output", &output)
        .await
        .unwrap();
    assert_eq!(
        native::Entity::find().one(&store.db).await.unwrap(),
        original
    );
    assert_eq!(quota::Entity::find().one(&store.db).await.unwrap(), charged);
    let mut changed = output.clone();
    changed.retention.delete_with_run = !changed.retention.delete_with_run;
    assert!(
        native_store
            .mark_succeeded("native-output", &changed)
            .await
            .is_err()
    );
    assert_eq!(
        native::Entity::find().one(&store.db).await.unwrap(),
        original
    );
    assert_eq!(quota::Entity::find().one(&store.db).await.unwrap(), charged);
}

#[tokio::test]
async fn missing_authorization_or_reverse_use_cannot_become_a_source_free_success() {
    for field in 0..2 {
        let (store, _, proof, _) = admitted(&format!("output-missing-{field}")).await;
        if field == 0 {
            authorization::Entity::delete_many()
                .exec(&store.db)
                .await
                .unwrap();
        } else {
            source_use::Entity::delete_many()
                .exec(&store.db)
                .await
                .unwrap();
        }
        assert!(matches!(
            SignalModelEgressStore::new(store.db.clone())
                .mark_succeeded("native-output", &output(&proof))
                .await,
            Err(ModelAdmissionError::AssistantDataChanged)
        ));
        let row = native::Entity::find()
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.state, crate::model_egress_store::STATE_DISPATCH_INTENT);
        assert!(row.assistant_source_set_json.is_some() && row.model_output_envelope_id.is_none());
        assert!(row.usage_json.is_some());
    }
}

#[tokio::test]
async fn completion_keeps_read_snapshots_but_rejects_changed_current_session_json() {
    for field in 0..2 {
        let (store, _, _, original) = admitted(&format!("output-final-{field}")).await;
        let txn = store.db.begin().await.unwrap();
        let row = native::Entity::find().one(&txn).await.unwrap().unwrap();
        let prepared =
            model_gate::output::prepare_on(&txn, row.assistant_source_set_json.as_deref())
                .await
                .unwrap()
                .unwrap();
        if field == 0 {
            origin::Entity::update_many()
                .col_expr(
                    origin::Column::DeletedAtMs,
                    Expr::value(Some(chrono::Utc::now().timestamp_millis())),
                )
                .filter(origin::Column::Id.eq(original))
                .exec(&txn)
                .await
                .unwrap();
        } else {
            let session = agent_session::Entity::find()
                .one(&txn)
                .await
                .unwrap()
                .unwrap();
            agent_session::Entity::update_many()
                .col_expr(
                    agent_session::Column::StateJson,
                    Expr::value(format!(" {}", session.state_json)),
                )
                .filter(agent_session::Column::Id.eq(session.id))
                .exec(&txn)
                .await
                .unwrap();
        }
        assert_eq!(
            model_gate::output::validate_on(&txn, Some(&prepared), &row)
                .await
                .is_err(),
            field != 0
        );
        txn.rollback().await.unwrap();
        assert!(
            native::Entity::find()
                .one(&store.db)
                .await
                .unwrap()
                .unwrap()
                .model_output_envelope_id
                .is_none()
        );
    }
}
