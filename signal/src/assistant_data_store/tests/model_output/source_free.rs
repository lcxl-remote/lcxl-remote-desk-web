use super::super::super::{output_archive, projection};
use super::*;
use crate::entity::{agent_assistant_model_output as archived, agent_memory_use as memory_use};
use desk_agent_protocol::ai_assistant::memory::MemoryDeleteRequest;
use desk_diagnose_core::{
    model_message_labels::model_bound_user_message, prompt::ResponseFormatSpec, seam::SessionSeam,
    session::PersistedAgentSession,
};
use sea_orm::PaginatorTrait;

mod derived;
mod visible_history;

type PlainAdmission = (
    TestStore,
    Box<PersistedAgentSession>,
    ModelRequest,
    desk_agent_protocol::data_lineage::DataEnvelope,
    ReadProjectionProof,
);

async fn admitted_plain(label: &str) -> PlainAdmission {
    admitted_plain_mode(label, false).await
}

async fn admitted_plain_mode(label: &str, needs_export: bool) -> PlainAdmission {
    let frame = prepare_plain(label, needs_export).await;
    capture_plain_input(&frame.0, &frame.1).await;
    frame
}

async fn capture_plain_input(store: &TestStore, session: &PersistedAgentSession) {
    let txn = crate::db::begin_write(&store.db, agent_session::Entity)
        .await
        .unwrap();
    let input = session
        .conversation
        .iter()
        .find(|message| message.message_id == "plain-owner-input")
        .unwrap();
    super::super::super::history::capture_user_input_on(
        &txn,
        session,
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
}

async fn prepare_plain(label: &str, needs_export: bool) -> PlainAdmission {
    let (store, _, unused, _) = super::super::model_gate::fixture_with_model(label, None).await;
    let raw = agent_session::Entity::find()
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let mut session = Box::new(PersistedAgentSession::decode_json(&raw.state_json).unwrap());
    let destination = unused.source_envelope.allowed_destinations[0].clone();
    let mut user = model_bound_user_message(
        "plain-owner-input".into(),
        "Answer an independent question".into(),
        destination.clone(),
    )
    .unwrap();
    if needs_export {
        let desk_agent_protocol::data_lineage::DestinationIdentity::Model { model_id, .. } =
            &mut user.data_envelope.as_mut().unwrap().allowed_destinations[0]
        else {
            panic!("expected model destination")
        };
        *model_id = format!("previous-{model_id}");
    }
    session.conversation.push(user.clone());
    session.version += 1;
    let changed = agent_session::Entity::update_many()
        .col_expr(agent_session::Column::Version, Expr::value(session.version))
        .col_expr(
            agent_session::Column::StateJson,
            Expr::value(session.encode_json_for_storage().unwrap()),
        )
        .filter(agent_session::Column::Id.eq(raw.id))
        .filter(agent_session::Column::ActorId.eq(&raw.actor_id))
        .filter(agent_session::Column::Version.eq(raw.version))
        .filter(agent_session::Column::LeaseToken.eq(raw.lease_token))
        .filter(agent_session::Column::StateJson.eq(raw.state_json))
        .exec(&store.db)
        .await
        .unwrap();
    assert_eq!(changed.rows_affected, 1);
    let mut request = ModelRequest::text_only(vec![user.clone()], ResponseFormatSpec::None);
    request.context_conversation_id = Some(session.conversation_id.clone());
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
    assert!(
        request
            .assistant_data
            .as_ref()
            .unwrap()
            .read_projection_ids
            .is_empty()
    );
    let policy = ModelEgressPolicy {
        destination,
        selected_source_tools: Default::default(),
        export_authorization_id: "plain-export".into(),
        now_unix_ms: chrono::Utc::now().timestamp_millis() as u64,
        byte_cap: desk_diagnose_core::sink_authorizer::MAX_SINK_BYTES,
        permission_resume: false,
    };
    let authorized = policy.authorize_request(request).unwrap();
    let input_snapshot = authorized.input_snapshot().unwrap();
    let request = authorized.request;
    let txn = store.db.begin().await.unwrap();
    let prepared = model_gate::prepare_on(&txn, &request)
        .await
        .unwrap()
        .unwrap();
    SignalModelEgressStore::record_snapshot_intent_on(
        &txn,
        "plain-native".into(),
        "plain-export".into(),
        1,
        &input_snapshot,
    )
    .await
    .unwrap();
    SignalModelEgressStore::link_context_dispatch_on(&txn, "plain-native", "root", false)
        .await
        .unwrap();
    model_gate::authorize_on(&txn, Some(&prepared), &request, "plain-native")
        .await
        .unwrap();
    txn.commit().await.unwrap();
    let sealed = native::Entity::find()
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let set =
        AssistantNativeSourceSet::decode(sealed.assistant_source_set_json.as_deref().unwrap())
            .unwrap();
    let before_replay = quota::Entity::find().one(&store.db).await.unwrap();
    let replay = store.db.begin().await.unwrap();
    assert!(
        native_export::record_sealed_call_on(&replay, &set, &[], &AssistantDataPolicy::default())
            .await
            .unwrap()
            .is_empty()
    );
    replay.commit().await.unwrap();
    assert_eq!(
        quota::Entity::find().one(&store.db).await.unwrap(),
        before_replay
    );
    SignalModelEgressStore::new(store.db.clone())
        .record_terminal_usage(
            "plain-native",
            &TokenUsage {
                input_tokens: Some(12),
                output_tokens: Some(4),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
    let candidate = policy
        .derive_model_output_envelope(
            &ModelTurn {
                text: "Independent answer without retrieved memory".into(),
                stop_reason: StopReason::EndTurn,
                ..Default::default()
            },
            &authorized.input_envelopes,
        )
        .unwrap();
    (store, session, request, candidate, unused)
}

#[tokio::test]
async fn original_request_manifest_survives_actual_model_export_and_archives_original_owner_label()
{
    let (store, _, request, candidate, unused) =
        admitted_plain_mode("plain-model-export", true).await;
    SignalModelEgressStore::new(store.db.clone())
        .mark_succeeded("plain-native", &candidate)
        .await
        .unwrap();
    let stored = output_archive::find_on(&store.db, "root", &candidate)
        .await
        .unwrap()
        .unwrap();
    let snapshot = &stored.proof.sources.input_snapshot;
    let original = request.messages[0].data_envelope.as_ref().unwrap();
    assert_eq!(snapshot.exports.len(), 1);
    assert_eq!(&snapshot.exports[0].source, original);
    assert_ne!(snapshot.envelopes[0].envelope_id, original.envelope_id);
    snapshot
        .validate_export_authorization("plain-export")
        .unwrap();
    stored.proof.sources.validate(&candidate).unwrap();
    stored
        .proof
        .input_policies
        .validate_registered_leaves(snapshot)
        .unwrap();
    assert!(
        stored
            .proof
            .input_policies
            .complete_leaf_text_policy(snapshot)
            .unwrap()
            .unwrap()
            .history_retention_allowed
    );
    assert_eq!(
        authorization::Entity::find()
            .count(&store.db)
            .await
            .unwrap(),
        0
    );
    assert!(
        projection::load_on(
            &store.db,
            &request.assistant_data.as_ref().unwrap().actor_id,
            &unused.usage.projection_id
        )
        .await
        .unwrap()
        .unwrap()
        .row
        .first_consumed_at_ms
        .is_none()
    );
    assert_eq!(
        SignalModelEgressStore::read_context_output_on(&store.db, "root", &candidate)
            .await
            .unwrap(),
        stored.proof.sources
    );
}

#[tokio::test]
async fn source_free_native_call_seals_original_input_without_consuming_unselected_memory_then_archives_without_body()
 {
    let (store, session, request, candidate, unused) = admitted_plain("plain-native-frame").await;
    let row = native::Entity::find()
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let set = AssistantNativeSourceSet::decode(row.assistant_source_set_json.as_deref().unwrap())
        .unwrap();
    assert!(set.authorizations.is_empty());
    set.validate_grants(&[]).unwrap();
    set.context.validate_session(&session).unwrap();
    assert_eq!(
        set.context.actor_input_seq,
        request.assistant_data.as_ref().unwrap().actor_input_seq
    );
    assert_eq!(
        authorization::Entity::find()
            .count(&store.db)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        memory_use::Entity::find().count(&store.db).await.unwrap(),
        0
    );
    let unused_before =
        projection::load_on(&store.db, &session.actor_id, &unused.usage.projection_id)
            .await
            .unwrap()
            .unwrap()
            .row;
    assert!(unused_before.first_consumed_at_ms.is_none());
    SignalModelEgressStore::new(store.db.clone())
        .mark_succeeded("plain-native", &candidate)
        .await
        .unwrap();
    let stored = output_archive::find_on(&store.db, "root", &candidate)
        .await
        .unwrap()
        .unwrap();
    stored.proof.validate_grants(&[]).unwrap();
    assert_eq!(
        stored.proof.input_policies.source_contract_revision,
        set.context.source_contract_revision
    );
    stored
        .proof
        .input_policies
        .validate_registered_leaves(&stored.proof.sources.input_snapshot)
        .unwrap();
    assert!(
        stored
            .proof
            .input_policies
            .complete_leaf_text_policy(&stored.proof.sources.input_snapshot)
            .unwrap()
            .unwrap()
            .history_retention_allowed
    );
    assert_eq!(
        stored.proof.input_recorded_at_unix_ms,
        set.context.input_recorded_at_unix_ms
    );
    assert!(
        !stored
            .row
            .proof_json
            .contains("Independent answer without retrieved memory")
    );
    assert!(
        !stored
            .row
            .proof_json
            .contains("Answer an independent question")
    );
    assert_eq!(
        projection::load_on(&store.db, &session.actor_id, &unused.usage.projection_id)
            .await
            .unwrap()
            .unwrap()
            .row,
        unused_before
    );
    let witnessed = SignalModelEgressStore::read_context_output_on(&store.db, "root", &candidate)
        .await
        .unwrap();
    native::Entity::delete_many().exec(&store.db).await.unwrap();
    assert_eq!(
        SignalModelEgressStore::read_context_output_on(&store.db, "root", &candidate)
            .await
            .unwrap(),
        witnessed
    );
    assert_eq!(archived::Entity::find().count(&store.db).await.unwrap(), 1);
    let mut message = desk_diagnose_core::chat::ChatMessage::text(
        "plain-answer",
        desk_diagnose_core::chat::ChatRole::Assistant,
        "Independent answer without retrieved memory",
    );
    message.data_envelope = Some(candidate);
    assert!(
        SignalModelEgressStore::read_output_evidence_on(
            &store.db,
            "plain-native",
            "plain-export",
            1,
            &message
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn unrelated_memory_forget_does_not_turn_a_source_free_answer_into_a_memory_dependent_one() {
    let (store, _, _, candidate, unused) = admitted_plain("plain-unrelated-forget").await;
    store
        .forget_memory(
            1,
            &MemoryDeleteRequest {
                memory_id: unused.usage.memories[0].memory_id.clone(),
                expected_revision: 1,
                operation_id: "forget-unused-by-call".into(),
            },
            &AssistantDataPolicy::default(),
            chrono::Utc::now().timestamp_millis() as u64,
        )
        .await
        .unwrap();
    SignalModelEgressStore::new(store.db.clone())
        .mark_succeeded("plain-native", &candidate)
        .await
        .unwrap();
    let stored = output_archive::find_on(&store.db, "root", &candidate)
        .await
        .unwrap()
        .unwrap();
    assert!(stored.proof.source_set.authorizations.is_empty());
    stored.proof.validate_grants(&[]).unwrap();
}

#[tokio::test]
async fn a_plain_output_cannot_be_attached_to_a_later_input_and_archive_capacity_failure_retains_real_usage()
 {
    for changed_input in [false, true] {
        let (store, mut session, _, candidate, _) =
            admitted_plain(&format!("plain-output-late-{changed_input}")).await;
        if changed_input {
            session.input_revision += 1;
            session.version += 1;
            session
                .begin_focus_epoch(session.input_revision, Vec::new())
                .unwrap();
            let txn = store.db.begin().await.unwrap();
            accept_input_on(
                &txn,
                &mut session,
                "UTC",
                None,
                chrono::Utc::now().timestamp_millis() as u64,
            )
            .await
            .unwrap();
            agent_session::Entity::update_many()
                .col_expr(
                    agent_session::Column::StateJson,
                    Expr::value(session.encode_json_for_storage().unwrap()),
                )
                .col_expr(agent_session::Column::Version, Expr::value(session.version))
                .exec(&txn)
                .await
                .unwrap();
            txn.commit().await.unwrap();
        } else {
            quota::Entity::update_many()
                .col_expr(
                    quota::Column::MetadataBytes,
                    Expr::value(AssistantDataPolicy::default().max_metadata_bytes as i64),
                )
                .exec(&store.db)
                .await
                .unwrap();
        }
        assert!(
            SignalModelEgressStore::new(store.db.clone())
                .mark_succeeded("plain-native", &candidate)
                .await
                .is_err()
        );
        assert_eq!(archived::Entity::find().count(&store.db).await.unwrap(), 0);
        let row = native::Entity::find()
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        assert!(row.model_output_envelope_id.is_none());
        let usage: TokenUsage = serde_json::from_str(row.usage_json.as_deref().unwrap()).unwrap();
        assert_eq!(
            (usage.input_tokens, usage.output_tokens),
            (Some(12), Some(4))
        );
    }
}
