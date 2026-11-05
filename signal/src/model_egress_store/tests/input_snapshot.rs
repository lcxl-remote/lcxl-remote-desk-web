use super::*;
use desk_agent_protocol::data_lineage::{ContentRef, Sensitivity};
use desk_diagnose_core::model_egress::NativeModelInputSnapshot;
use sea_orm::{ColumnTrait, QueryFilter, sea_query::Expr};

#[tokio::test]
async fn actual_authorizer_snapshot_records_original_export_sources_and_does_not_accept_another_dispatch_authority()
 {
    use desk_diagnose_core::{
        chat::{ChatMessage, ModelTurn, StopReason, ToolCall},
        model_egress::ModelEgressPolicy,
        model_message_labels::{ReadResultLabel, read_result_envelope},
        prompt::ResponseFormatSpec,
        seam::{ModelRequest, ToolOutputFormat, ToolRunOutput},
    };
    let db = crate::config::test_support::Database::connect("sqlite::memory:")
        .await
        .unwrap();
    crate::db::initialize_schema(&db).await.unwrap();
    let tool = desk_diagnose_core::web_research::WEB_FETCH_TOOL_NAME;
    let result = ToolRunOutput {
        format: ToolOutputFormat::Text,
        content: "bounded source".into(),
        image_data_url: None,
        document_preview: None,
    };
    let mut source = read_result_envelope(
        desk_diagnose_core::ai_assistant::ai_assistant_provider_registry_ref(),
        &ToolCall {
            id: "read".into(),
            name: tool.into(),
            arguments_json: "{}".into(),
        },
        &result,
        ReadResultLabel {
            envelope_id: "source".into(),
            observation_id: "observation".into(),
            source_object_id: None,
            observed_at_unix_ms: 100,
        },
    )
    .unwrap();
    source.provenance.source_envelope_ids = vec!["background-source".into()];
    let mut message = ChatMessage::tool_result("result-message", "read", result.content);
    message.data_envelope = Some(source.clone());
    let policy = ModelEgressPolicy {
        destination: audit().destination,
        selected_source_tools: [tool.into()].into_iter().collect(),
        export_authorization_id: "actual-export".into(),
        now_unix_ms: 100,
        byte_cap: desk_diagnose_core::sink_authorizer::MAX_SINK_BYTES,
        permission_resume: false,
    };
    let authorized = policy
        .authorize_request(ModelRequest::text_only(
            vec![message],
            ResponseFormatSpec::None,
        ))
        .unwrap();
    let snapshot = authorized.input_snapshot().unwrap();
    let txn = crate::db::begin_write(&db, model_egress_receipt::Entity)
        .await
        .unwrap();
    let row = SignalModelEgressStore::record_snapshot_intent_on(
        &txn,
        "export-source".into(),
        "actual-export".into(),
        1,
        &snapshot,
    )
    .await
    .unwrap();
    SignalModelEgressStore::link_context_dispatch_on(&txn, "export-source", "root", false)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    assert_eq!(receipt_inputs(&row).unwrap().2, snapshot);
    let candidate = policy
        .derive_model_output_envelope(
            &ModelTurn {
                text: "conclusion".into(),
                stop_reason: StopReason::EndTurn,
                ..Default::default()
            },
            &authorized.input_envelopes,
        )
        .unwrap();
    SignalModelEgressStore::new(db.clone())
        .mark_succeeded("export-source", &candidate)
        .await
        .unwrap();
    let witnessed = SignalModelEgressStore::read_context_output_on(&db, "root", &candidate)
        .await
        .unwrap();
    assert_eq!(witnessed.input_snapshot.exports[0].source, source);
    witnessed.validate(&candidate).unwrap();
    let txn = crate::db::begin_write(&db, model_egress_receipt::Entity)
        .await
        .unwrap();
    assert!(
        SignalModelEgressStore::record_snapshot_intent_on(
            &txn,
            "wrong-export-source".into(),
            "different-export".into(),
            2,
            &snapshot
        )
        .await
        .is_err()
    );
    txn.rollback().await.unwrap();
    assert!(
        model_egress_receipt::Entity::find_by_id("wrong-export-source")
            .one(&db)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn dispatch_receipt_retains_exact_labels_and_its_digest_includes_original_constraints() {
    let db = crate::config::test_support::Database::connect("sqlite::memory:")
        .await
        .unwrap();
    crate::db::initialize_schema(&db).await.unwrap();
    let store = SignalModelEgressStore::new(db.clone());
    let sink = audit();
    let original = test_inputs(&sink);
    let row = store
        .record_dispatch_intent(
            "snapshot-original".into(),
            "export".into(),
            1,
            &sink,
            &original,
        )
        .await
        .unwrap();
    let (_, lineage, snapshot) = receipt_inputs(&row).unwrap();
    assert_eq!(snapshot.envelopes, original);
    assert_eq!(
        serde_json::from_str::<NativeModelInputSnapshot>(&row.input_snapshot_json).unwrap(),
        snapshot
    );
    for case in 0..6 {
        let mut changed = original.clone();
        match case {
            0 => changed[0].sensitivity = Sensitivity::Sensitive,
            1 => changed[0].retention.expires_at_unix_ms = Some(1000),
            2 => changed[0].retention.delete_with_run = false,
            3 => changed[0].provenance.source_object_id = Some("other-object".into()),
            4 => {
                let ContentRef::ImmutableBlob { blob_id, .. } = &mut changed[0].content else {
                    panic!("fixture blob")
                };
                *blob_id = "other-content-reference".into();
            }
            _ => changed[0]
                .allowed_destinations
                .push(DestinationIdentity::Model {
                    connection_id: "other".into(),
                    connection_revision: 1,
                    model_id: "other-model".into(),
                    profile_revision: 1,
                }),
        }
        let next = store
            .record_dispatch_intent(
                format!("snapshot-{case}"),
                "export".into(),
                case + 2,
                &sink,
                &changed,
            )
            .await
            .unwrap();
        assert_eq!(receipt_inputs(&next).unwrap().1, lineage);
        assert_ne!(next.projection_digest_sha256, row.projection_digest_sha256);
    }
}

#[tokio::test]
async fn changed_or_missing_snapshot_cannot_complete_or_reconstruct_a_native_output() {
    let db = crate::config::test_support::Database::connect("sqlite::memory:")
        .await
        .unwrap();
    crate::db::initialize_schema(&db).await.unwrap();
    let store = SignalModelEgressStore::new(db.clone());
    let row = store
        .record_dispatch_intent(
            "snapshot-tamper".into(),
            "export".into(),
            1,
            &audit(),
            &test_inputs(&audit()),
        )
        .await
        .unwrap();
    store
        .link_context_dispatch("snapshot-tamper", "conversation")
        .await
        .unwrap();
    for case in 0..3 {
        let mut changed = receipt_inputs(&row).unwrap().2;
        changed.envelopes[0].retention.expires_at_unix_ms = Some(1000);
        let encoded = match case {
            0 => serde_json::to_string(&changed).unwrap(),
            1 => "{}".into(),
            _ => "null".into(),
        };
        model_egress_receipt::Entity::update_many()
            .col_expr(
                model_egress_receipt::Column::InputSnapshotJson,
                Expr::value(encoded),
            )
            .filter(model_egress_receipt::Column::ReceiptId.eq("snapshot-tamper"))
            .exec(&db)
            .await
            .unwrap();
        let damaged = model_egress_receipt::Entity::find_by_id("snapshot-tamper")
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert!(receipt_inputs(&damaged).is_err());
        assert!(
            store
                .mark_succeeded("snapshot-tamper", &output("output"))
                .await
                .is_err()
        );
        assert!(
            SignalModelEgressStore::read_context_output_on(&db, "conversation", &output("output"))
                .await
                .is_err()
        );
        assert_eq!(
            model_egress_receipt::Entity::find_by_id("snapshot-tamper")
                .one(&db)
                .await
                .unwrap()
                .unwrap(),
            damaged
        );
    }
}
