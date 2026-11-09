use super::super::{native_export, output_archive};
use super::model_output::{admitted, output};
use super::*;
use crate::entity::{
    agent_assistant_data_authorization as authorization, agent_assistant_model_output as archived,
    agent_assistant_quota as quota, model_egress_receipt as native,
};
use crate::model_egress_store::SignalModelEgressStore;
use desk_agent_protocol::ai_assistant::memory::MemoryDeleteRequest;
use sea_orm::{PaginatorTrait, sea_query::Expr};

mod order;

#[tokio::test]
async fn archived_output_preserves_original_input_and_provenance_without_response_or_memory_bodies()
{
    let (store, request, proof, _) = admitted("archived-output").await;
    let candidate = output(&proof);
    let before = quota::Entity::find().one(&store.db).await.unwrap().unwrap();
    SignalModelEgressStore::new(store.db.clone())
        .mark_succeeded("native-output", &candidate)
        .await
        .unwrap();
    let stored = output_archive::find_on(&store.db, "root", &candidate)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        stored.proof.actor_input_seq,
        request.assistant_data.as_ref().unwrap().actor_input_seq
    );
    assert_eq!(stored.proof.statement_timezone, "UTC");
    assert_eq!(
        stored.proof.sources.input_snapshot.envelopes,
        vec![proof.source_envelope.clone()]
    );
    stored.proof.sources.validate(&candidate).unwrap();
    assert!(
        !stored
            .row
            .proof_json
            .contains("Completed using the selected memory")
    );
    assert!(!stored.row.proof_json.contains("Prefer Chinese"));
    assert_eq!(
        quota::Entity::find()
            .one(&store.db)
            .await
            .unwrap()
            .unwrap()
            .metadata_bytes
            - before.metadata_bytes,
        output_archive::metadata_bytes(&stored.row).unwrap()
            + crate::entity::agent_assistant_model_input_item::Entity::find()
                .all(&store.db)
                .await
                .unwrap()
                .iter()
                .map(
                    |row| desk_assistant_data_store::output_store::item_metadata_bytes(row)
                        .unwrap()
                )
                .sum::<i64>()
    );
    let after = quota::Entity::find().one(&store.db).await.unwrap().unwrap();
    assert!(
        SignalModelEgressStore::new(store.db.clone())
            .mark_succeeded("native-output", &candidate)
            .await
            .is_ok()
    );
    assert_eq!(
        quota::Entity::find().one(&store.db).await.unwrap().unwrap(),
        after
    );
    assert_eq!(archived::Entity::find().count(&store.db).await.unwrap(), 1);
    let grants = native_export::load_call_set_on(&store.db, &stored.proof.source_set)
        .await
        .unwrap()
        .into_iter()
        .map(|grant| *grant.proof)
        .collect::<Vec<_>>();
    stored.proof.validate_grants(&grants).unwrap();
    for field in 0..7 {
        let mut changed = stored.proof.as_ref().clone();
        match field {
            0 => changed.actor_input_seq += 1,
            1 => changed.input_revision += 1,
            2 => changed.sources.inputs.clear(),
            3 => changed.output.retention.delete_with_run = false,
            4 => changed.sources.input_snapshot.envelopes.clear(),
            5 => changed.input_policies.leaves.clear(),
            _ => changed.source_set.context.source_contract_revision += 1,
        }
        assert!(changed.validate().is_err(), "field {field}");
    }
}

#[tokio::test]
async fn metadata_survives_native_cleanup_but_does_not_renew_source_access_or_original_execution_authority()
 {
    let (store, _, proof, _) = admitted("archive-after-native-gc").await;
    let candidate = output(&proof);
    SignalModelEgressStore::new(store.db.clone())
        .mark_succeeded("native-output", &candidate)
        .await
        .unwrap();
    let before = SignalModelEgressStore::read_context_output_on(&store.db, "root", &candidate)
        .await
        .unwrap();
    native::Entity::delete_many().exec(&store.db).await.unwrap();
    assert_eq!(
        SignalModelEgressStore::read_context_output_on(&store.db, "root", &candidate)
            .await
            .unwrap(),
        before
    );
    let mut message = desk_diagnose_core::chat::ChatMessage::text(
        "answer",
        desk_diagnose_core::chat::ChatRole::Assistant,
        "Completed using the selected memory",
    );
    message.data_envelope = Some(candidate.clone());
    assert!(
        SignalModelEgressStore::read_output_evidence_on(
            &store.db,
            "native-output",
            "export",
            1,
            &message
        )
        .await
        .is_err()
    );
    store
        .forget_memory(
            1,
            &MemoryDeleteRequest {
                memory_id: proof.usage.memories[0].memory_id.clone(),
                expected_revision: 1,
                operation_id: "forget-archived-source".into(),
            },
            &AssistantDataPolicy::default(),
            chrono::Utc::now().timestamp_millis() as u64,
        )
        .await
        .unwrap();
    let stored = output_archive::find_on(&store.db, "root", &candidate)
        .await
        .unwrap()
        .unwrap();
    assert!(
        native_export::load_call_set_on(&store.db, &stored.proof.source_set)
            .await
            .is_err()
    );
    assert_eq!(
        SignalModelEgressStore::read_context_output_on(&store.db, "root", &candidate)
            .await
            .unwrap(),
        before
    );
    assert_eq!(native::Entity::find().count(&store.db).await.unwrap(), 0);
    assert_eq!(
        authorization::Entity::find()
            .count(&store.db)
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn archive_capacity_failure_keeps_usage_but_commits_neither_output_acceptance_nor_archive() {
    let (store, _, proof, _) = admitted("archive-capacity").await;
    let limit = AssistantDataPolicy::default().max_metadata_bytes as i64;
    quota::Entity::update_many()
        .col_expr(quota::Column::MetadataBytes, Expr::value(limit))
        .exec(&store.db)
        .await
        .unwrap();
    let before = quota::Entity::find().one(&store.db).await.unwrap().unwrap();
    assert!(
        SignalModelEgressStore::new(store.db.clone())
            .mark_succeeded("native-output", &output(&proof))
            .await
            .is_err()
    );
    assert_eq!(archived::Entity::find().count(&store.db).await.unwrap(), 0);
    let row = native::Entity::find()
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.state, crate::model_egress_store::STATE_DISPATCH_INTENT);
    assert!(row.model_output_envelope_id.is_none());
    let usage: desk_diagnose_core::chat::TokenUsage =
        serde_json::from_str(row.usage_json.as_deref().unwrap()).unwrap();
    assert_eq!(
        (usage.input_tokens, usage.output_tokens),
        (Some(12), Some(4))
    );
    assert_eq!(
        quota::Entity::find().one(&store.db).await.unwrap().unwrap(),
        before
    );
}

#[tokio::test]
async fn corrupted_archives_and_contradictory_live_receipts_fail_closed() {
    for field in 0..4 {
        let (store, _, proof, _) = admitted(&format!("archive-corrupt-{field}")).await;
        let candidate = output(&proof);
        SignalModelEgressStore::new(store.db.clone())
            .mark_succeeded("native-output", &candidate)
            .await
            .unwrap();
        if field == 0 {
            let row = archived::Entity::find()
                .one(&store.db)
                .await
                .unwrap()
                .unwrap();
            let mut json: serde_json::Value = serde_json::from_str(&row.proof_json).unwrap();
            json["actor_input_seq"] = serde_json::json!(9999);
            archived::Entity::update_many()
                .col_expr(archived::Column::ProofJson, Expr::value(json.to_string()))
                .exec(&store.db)
                .await
                .unwrap();
        } else if field == 1 {
            native::Entity::update_many()
                .col_expr(
                    native::Column::State,
                    Expr::value(crate::model_egress_store::STATE_FAILED),
                )
                .col_expr(
                    native::Column::ModelOutputEnvelopeId,
                    Expr::value(Option::<String>::None),
                )
                .exec(&store.db)
                .await
                .unwrap();
        } else if field == 2 {
            let row = archived::Entity::find()
                .one(&store.db)
                .await
                .unwrap()
                .unwrap();
            let mut duplicate: archived::ActiveModel = row.into();
            duplicate.id = sea_orm::ActiveValue::NotSet;
            duplicate.physical_call_id = Set("conflicting-output-copy".into());
            duplicate.insert(&store.db).await.unwrap();
        } else {
            let row = native::Entity::find()
                .one(&store.db)
                .await
                .unwrap()
                .unwrap();
            let mut set =
                desk_diagnose_core::ai_assistant::native_export::AssistantNativeSourceSet::decode(
                    row.assistant_source_set_json.as_deref().unwrap(),
                )
                .unwrap();
            set.actor_id = "other-owner".into();
            native::Entity::update_many()
                .col_expr(
                    native::Column::AssistantSourceSetJson,
                    Expr::value(Some(serde_json::to_string(&set).unwrap())),
                )
                .exec(&store.db)
                .await
                .unwrap();
        }
        assert!(
            SignalModelEgressStore::read_context_output_on(&store.db, "root", &candidate)
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn decoded_archive_and_read_reuse_still_reject_raw_changes_revocation_and_missing_uses() {
    let (store, _, proof, _) = admitted("decoded-reuse").await;
    let candidate = output(&proof);
    SignalModelEgressStore::new(store.db.clone())
        .mark_succeeded("native-output", &candidate)
        .await
        .unwrap();
    let stored = output_archive::find_on(&store.db, "root", &candidate)
        .await
        .unwrap()
        .unwrap();
    let reads =
        native_export::load_call_set_with_projections_on(&store.db, &stored.proof.source_set)
            .await
            .unwrap();
    assert!(!reads.is_empty());
    assert!(
        native_export::load_call_set_with_projections_on(&store.db, &stored.proof.source_set)
            .await
            .is_ok()
    );
    let reused = output_archive::find_reusing_on(&store.db, "root", &candidate, Some(&stored))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reused.row, stored.row);
    assert_eq!(reused.proof, stored.proof);
    use crate::entity::agent_assistant_read_projection as projection;
    for mutation in 0..4 {
        let txn = store.db.begin().await.unwrap();
        match mutation {
            0 => {
                authorization::Entity::update_many()
                    .col_expr(
                        authorization::Column::RevokedAtMs,
                        Expr::value(Some(2000_i64)),
                    )
                    .filter(authorization::Column::Id.eq(reads[0].0.row.id))
                    .exec(&txn)
                    .await
                    .unwrap();
            }
            1 => {
                projection::Entity::update_many()
                    .col_expr(projection::Column::RevokedAtMs, Expr::value(Some(2000_i64)))
                    .filter(projection::Column::Id.eq(reads[0].1.row.id))
                    .exec(&txn)
                    .await
                    .unwrap();
            }
            2 => {
                projection::Entity::update_many()
                    .col_expr(
                        projection::Column::ResultJson,
                        Expr::value(Some("{corrupt".to_string())),
                    )
                    .filter(projection::Column::Id.eq(reads[0].1.row.id))
                    .exec(&txn)
                    .await
                    .unwrap();
            }
            _ => {
                use crate::entity::{agent_assistant_source_use, agent_memory_use};
                let sources = agent_assistant_source_use::Entity::delete_many()
                    .exec(&txn)
                    .await
                    .unwrap();
                let memories = agent_memory_use::Entity::delete_many()
                    .exec(&txn)
                    .await
                    .unwrap();
                assert!(sources.rows_affected + memories.rows_affected > 0);
            }
        }
        assert!(
            native_export::load_call_set_with_projections_on(&txn, &stored.proof.source_set)
                .await
                .is_err()
        );
        txn.rollback().await.unwrap();
    }
    let txn = store.db.begin().await.unwrap();
    archived::Entity::update_many()
        .filter(archived::Column::Id.eq(stored.row.id))
        .col_expr(
            archived::Column::ProofJson,
            Expr::value("{corrupt".to_string()),
        )
        .exec(&txn)
        .await
        .unwrap();
    assert!(
        output_archive::find_reusing_on(&txn, "root", &candidate, Some(&stored))
            .await
            .is_err()
    );
    assert!(
        output_archive::find_on(&txn, "root", &candidate)
            .await
            .is_err()
    );
    txn.rollback().await.unwrap();
}
