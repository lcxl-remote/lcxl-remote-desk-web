use super::super::super::{model_gate, native_export};
use super::*;
use crate::model_egress_store::SignalModelEgressStore;
use desk_diagnose_core::{ai_assistant::native_export::AssistantNativeSourceSet, chat::TokenUsage};

#[tokio::test]
async fn actual_current_preparation_requires_a_manifest_and_its_first_send_and_output_use_the_original_native_receipt()
 {
    let (fixture, request, proof) = ready_request("current-native-dispatch").await;
    let mut absent = request.clone();
    absent.assistant_data = None;
    let txn = fixture.store.db.begin().await.unwrap();
    assert!(model_gate::prepare_on(&txn, &absent).await.is_err());
    txn.rollback().await.unwrap();
    let txn = fixture.store.db.begin().await.unwrap();
    let prepared = model_gate::prepare_on(&txn, &request)
        .await
        .unwrap()
        .unwrap();
    super::super::model_gate::native_on(&txn, &request, &proof, "actual-current-send").await;
    model_gate::authorize_on(&txn, Some(&prepared), &request, "actual-current-send")
        .await
        .unwrap();
    txn.commit().await.unwrap();
    let row = native::Entity::find_by_id("actual-current-send")
        .one(&fixture.store.db)
        .await
        .unwrap()
        .unwrap();
    let set = AssistantNativeSourceSet::decode(row.assistant_source_set_json.as_deref().unwrap())
        .unwrap();
    assert_eq!(set.authorizations.len(), 1);
    let grant = native_export::load_call_set_on(&fixture.store.db, &set)
        .await
        .unwrap();
    assert_eq!(grant[0].proof.projection_id, proof.usage.projection_id);
    assert_eq!(
        grant[0].proof.source_envelope_id,
        proof.source_envelope.envelope_id
    );
    assert!(
        projection::load_on(
            &fixture.store.db,
            &fixture.session.actor_id,
            &proof.usage.projection_id
        )
        .await
        .unwrap()
        .unwrap()
        .row
        .first_consumed_at_ms
        .is_some()
    );
    let store = SignalModelEgressStore::new(fixture.store.db.clone());
    store
        .record_terminal_usage(
            "actual-current-send",
            &TokenUsage {
                input_tokens: Some(7),
                output_tokens: Some(3),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
    let output = super::super::model_output::output(&proof);
    store
        .mark_succeeded("actual-current-send", &output)
        .await
        .unwrap();
    let row = native::Entity::find_by_id("actual-current-send")
        .one(&fixture.store.db)
        .await
        .unwrap()
        .unwrap();
    let usage: TokenUsage = serde_json::from_str(row.usage_json.as_deref().unwrap()).unwrap();
    assert_eq!(
        (usage.input_tokens, usage.output_tokens),
        (Some(7), Some(3))
    );
    assert_eq!(
        row.model_output_envelope_id.as_deref(),
        Some(output.envelope_id.as_str())
    );
    let archive = super::super::super::output_archive::find_on(
        &fixture.store.db,
        &fixture.session.conversation_id,
        &output,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(archive.proof.source_set, set);
}

#[tokio::test]
async fn current_body_or_source_changes_before_original_send_admission_roll_back_the_native_row_and_read_consumption()
 {
    for variant in 0..2 {
        let (fixture, mut request, proof) =
            ready_request(&format!("current-native-reject-{variant}")).await;
        if variant == 0 {
            request.messages[0]
                .text
                .push_str("altered after preparation");
        }
        let before = quota::Entity::find()
            .one(&fixture.store.db)
            .await
            .unwrap()
            .unwrap();
        let txn = fixture.store.db.begin().await.unwrap();
        let prepared = model_gate::prepare_on(&txn, &request)
            .await
            .unwrap()
            .unwrap();
        super::super::model_gate::native_on(&txn, &request, &proof, "changed-current-send").await;
        if variant == 1 {
            let source = &proof.usage.history_dependencies[0];
            origin::Entity::update_many()
                .col_expr(origin::Column::DeletedAtMs, Expr::value(1_i64))
                .filter(origin::Column::OriginId.eq(&source.origin_id))
                .exec(&txn)
                .await
                .unwrap();
        }
        assert!(
            model_gate::authorize_on(&txn, Some(&prepared), &request, "changed-current-send")
                .await
                .is_err()
        );
        txn.rollback().await.unwrap();
        assert_eq!(
            native::Entity::find()
                .count(&fixture.store.db)
                .await
                .unwrap(),
            0
        );
        assert!(
            projection::load_on(
                &fixture.store.db,
                &fixture.session.actor_id,
                &proof.usage.projection_id
            )
            .await
            .unwrap()
            .unwrap()
            .row
            .first_consumed_at_ms
            .is_none()
        );
        assert_eq!(
            quota::Entity::find()
                .one(&fixture.store.db)
                .await
                .unwrap()
                .unwrap(),
            before
        );
    }
}
