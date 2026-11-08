use super::*;
use desk_agent_protocol::data_lineage::{DataEnvelope, DestinationIdentity};
use desk_diagnose_core::chat::{ChatMessage, ChatRole};

mod memory;
mod parents;

async fn admit_followup(
    store: &TestStore,
    session: &mut PersistedAgentSession,
    parent: &DataEnvelope,
    parent_text: &str,
) -> (ModelRequest, DataEnvelope) {
    let mut destination = parent.allowed_destinations[0].clone();
    let DestinationIdentity::Model { model_id, .. } = &mut destination else {
        panic!("expected model destination")
    };
    *model_id = "followup-model".into();
    let mut answer = ChatMessage::text("previous-answer", ChatRole::Assistant, parent_text);
    answer.data_envelope = Some(parent.clone());
    let user = model_bound_user_message(
        "followup-input".into(),
        "Explain the previous answer".into(),
        destination.clone(),
    )
    .unwrap();
    session.input_revision += 1;
    session
        .begin_focus_epoch(session.input_revision, Vec::new())
        .unwrap();
    session.conversation.extend([answer.clone(), user.clone()]);
    let txn = store.db.begin().await.unwrap();
    let raw = agent_session::Entity::find()
        .one(&txn)
        .await
        .unwrap()
        .unwrap();
    accept_input_on(
        &txn,
        session,
        "UTC",
        None,
        chrono::Utc::now().timestamp_millis() as u64,
    )
    .await
    .unwrap();
    super::super::super::super::history::capture_user_input_on(
        &txn,
        session,
        &user,
        session
            .assistant_data
            .as_ref()
            .unwrap()
            .current_input_recorded_at_unix_ms,
    )
    .await
    .unwrap();
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
        .exec(&txn)
        .await
        .unwrap();
    assert_eq!(changed.rows_affected, 1);
    txn.commit().await.unwrap();
    let mut request = ModelRequest::text_only(vec![answer, user], ResponseFormatSpec::None);
    request.context_conversation_id = Some(session.conversation_id.clone());
    let host = crate::agent_session_store::SignalAgentSessionStore::new(store.db.clone());
    request.assistant_data = host
        .assistant_data_request_manifest(
            session,
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
        export_authorization_id: "followup-export".into(),
        now_unix_ms: chrono::Utc::now().timestamp_millis() as u64,
        byte_cap: desk_diagnose_core::sink_authorizer::MAX_SINK_BYTES,
        permission_resume: false,
    };
    let authorized = policy.authorize_request(request).unwrap();
    let snapshot = authorized.input_snapshot().unwrap();
    let request = authorized.request;
    let txn = store.db.begin().await.unwrap();
    let prepared = model_gate::prepare_on(&txn, &request)
        .await
        .unwrap()
        .unwrap();
    SignalModelEgressStore::record_snapshot_intent_on(
        &txn,
        "followup-native".into(),
        "followup-export".into(),
        2,
        &snapshot,
    )
    .await
    .unwrap();
    SignalModelEgressStore::link_context_dispatch_on(&txn, "followup-native", "root", false)
        .await
        .unwrap();
    model_gate::authorize_on(&txn, Some(&prepared), &request, "followup-native")
        .await
        .unwrap();
    txn.commit().await.unwrap();
    SignalModelEgressStore::new(store.db.clone())
        .record_terminal_usage(
            "followup-native",
            &TokenUsage {
                input_tokens: Some(13),
                output_tokens: Some(5),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
    let candidate = policy
        .derive_model_output_envelope(
            &ModelTurn {
                text: "Explanation of the earlier answer".into(),
                stop_reason: StopReason::EndTurn,
                ..Default::default()
            },
            &authorized.input_envelopes,
        )
        .unwrap();
    (request, candidate)
}

#[tokio::test]
async fn followup_archive_preserves_original_policy_through_actual_exports_and_after_original_receipt_is_absent()
 {
    for remove_native in [false, true] {
        let (store, mut session, _, parent, _) =
            admitted_plain_mode(&format!("derived-native-{remove_native}"), true).await;
        SignalModelEgressStore::new(store.db.clone())
            .mark_succeeded("plain-native", &parent)
            .await
            .unwrap();
        let original = output_archive::find_on(&store.db, "root", &parent)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(original.proof.sources.input_snapshot.exports.len(), 1);
        if remove_native {
            native::Entity::delete_by_id("plain-native")
                .exec(&store.db)
                .await
                .unwrap();
        }
        let (_, candidate) = admit_followup(
            &store,
            &mut session,
            &parent,
            "Independent answer without retrieved memory",
        )
        .await;
        SignalModelEgressStore::new(store.db.clone())
            .mark_succeeded("followup-native", &candidate)
            .await
            .unwrap();
        let stored = output_archive::find_on(&store.db, "root", &candidate)
            .await
            .unwrap()
            .unwrap();
        let graph = &stored.proof.input_policy_graph;
        assert_eq!(graph.parents.len(), 1);
        assert_eq!(
            graph.parents[0].physical_call_id,
            original.row.physical_call_id
        );
        graph.parents[0].validate_archive(&original.proof).unwrap();
        assert_eq!(
            graph.parents[0].actor_input_seq,
            original.proof.actor_input_seq
        );
        assert_ne!(
            graph.parents[0].actor_input_seq,
            stored.proof.actor_input_seq
        );
        assert!(
            stored
                .proof
                .sources
                .input_snapshot
                .exports
                .iter()
                .any(|export| export.source == parent)
        );
        assert!(
            stored
                .proof
                .input_policies
                .complete_leaf_text_policy(&stored.proof.sources.input_snapshot)
                .unwrap()
                .is_none()
        );
        assert!(
            graph
                .complete_text_policy(
                    &stored.proof.sources.input_snapshot,
                    &stored.proof.input_policies
                )
                .unwrap()
                .unwrap()
                .history_retention_allowed
        );
        assert!(stored.proof.source_set.authorizations.is_empty());
        assert!(
            !stored
                .row
                .proof_json
                .contains("Explanation of the earlier answer")
        );
        assert!(
            !stored
                .row
                .proof_json
                .contains("Independent answer without retrieved memory")
        );
        assert_eq!(
            authorization::Entity::find()
                .count(&store.db)
                .await
                .unwrap(),
            0
        );
    }
}

#[tokio::test]
async fn contradictory_live_parent_receipt_cannot_be_hidden_by_its_valid_archive_and_followup_usage_is_retained()
 {
    let (store, mut session, _, parent, _) = admitted_plain("derived-contradiction").await;
    SignalModelEgressStore::new(store.db.clone())
        .mark_succeeded("plain-native", &parent)
        .await
        .unwrap();
    let (_, candidate) = admit_followup(
        &store,
        &mut session,
        &parent,
        "Independent answer without retrieved memory",
    )
    .await;
    native::Entity::update_many()
        .col_expr(
            native::Column::State,
            Expr::value(crate::model_egress_store::STATE_FAILED),
        )
        .filter(native::Column::ReceiptId.eq("plain-native"))
        .exec(&store.db)
        .await
        .unwrap();
    let before = quota::Entity::find().one(&store.db).await.unwrap();
    assert!(
        SignalModelEgressStore::new(store.db.clone())
            .mark_succeeded("followup-native", &candidate)
            .await
            .is_err()
    );
    assert_eq!(quota::Entity::find().one(&store.db).await.unwrap(), before);
    assert!(
        output_archive::find_on(&store.db, "root", &candidate)
            .await
            .unwrap()
            .is_none()
    );
    let row = native::Entity::find_by_id("followup-native")
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let usage: TokenUsage = serde_json::from_str(row.usage_json.as_deref().unwrap()).unwrap();
    assert_eq!(
        (usage.input_tokens, usage.output_tokens),
        (Some(13), Some(5))
    );
    assert!(row.model_output_envelope_id.is_none());
}
