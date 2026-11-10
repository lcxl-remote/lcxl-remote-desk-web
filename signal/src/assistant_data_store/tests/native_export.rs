use super::super::{
    native_export::{load_on, memory_use_bytes, metadata_bytes, record_on, source_use_bytes},
    projection::{load_on as load_projection_on, prepare_on},
};
use super::*;
use crate::entity::{
    agent_assistant_data_authorization as authorization,
    agent_assistant_read_projection as projection, agent_assistant_source_use as source_use,
    agent_memory_use as memory_use, model_egress_receipt as native,
};
use desk_agent_protocol::ai_assistant::memory::MemoryUsePurpose;
use desk_agent_protocol::ai_assistant::source::{HistoryDependency, SourceReadPurpose};
use desk_diagnose_core::{
    ai_assistant::{
        data_control::CurrentMemoryUse,
        native_export::{
            AssistantDataExportAuthorization, MAX_NATIVE_EXPORT_TTL_MS, NativeAssistantModelCall,
        },
        read_projection::ReadProjectionProof,
    },
    native_ids::{self, Kind},
    sink_authorizer::SinkProjectionAudit,
};
use sea_orm::PaginatorTrait;

// Storage tests deliberately omit live source/task authority. Production callers
// must validate those under their original fences before recording a native use.
fn source_proof(key: &str) -> ReadProjectionProof {
    let mut proof = super::projection::proof("{}", key);
    let now = chrono::Utc::now().timestamp_millis() as u64;
    proof.created_at_unix_ms = now - 1000;
    proof.unused_until_unix_ms = now + 60_000;
    proof.usage.history_dependencies.push(HistoryDependency {
        source_id: native_ids::fixture_source_id(&"source"),
        source_revision: 1,
        origin_id: native_ids::fixture_origin_id(&"origin"),
        origin_deletion_generation: 1,
        policy_revision: 1,
        expires_at_unix_ms: None,
    });
    proof
        .usage
        .roots
        .push(desk_diagnose_core::ai_assistant::seam::AssistantReadRoot {
            source_id: proof.usage.history_dependencies[0].source_id.clone(),
            source_revision: 1,
        });
    proof.usage.memories.push(CurrentMemoryUse {
        memory_id: native_ids::stable(Kind::Memory, &"memory"),
        revision: 1,
        purpose: MemoryUsePurpose::CurrentFact,
    });
    proof
}

fn grant(
    proof: &ReadProjectionProof,
    call: NativeAssistantModelCall,
    now: u64,
) -> AssistantDataExportAuthorization {
    let purpose = SourceReadPurpose::ModelMemory;
    let value = AssistantDataExportAuthorization {
        authorization_id: native_ids::stable(
            Kind::AssistantReadAuthorization,
            &(
                "assistant-native-export-v1",
                &proof.actor_id,
                &proof.root_conversation_id,
                &call.physical_call_id,
                &proof.usage.projection_id,
                purpose,
            ),
        ),
        actor_id: proof.actor_id.clone(),
        consumer_conversation_id: proof.root_conversation_id.clone(),
        source_device_ref: proof.source_device_ref.clone(),
        session_version: 1,
        lease_token: 1,
        input_revision: proof.input_revision,
        actor_input_seq: proof.actor_input_seq,
        input_recorded_at_unix_ms: proof.created_at_unix_ms,
        statement_timezone: "UTC".into(),
        retrieval_episode_id: proof.usage.retrieval_episode_id.clone(),
        task_id: None,
        scope: proof.usage.scope.clone(),

        policy_revision: 1,
        source_contract_revision:
            desk_diagnose_core::ai_assistant::source_registry::SOURCE_CONTRACT_REVISION,
        memory_state_generation: 1,
        control_generation: 1,
        projection_id: proof.usage.projection_id.clone(),
        source_envelope_id: proof.source_envelope.envelope_id.clone(),
        purpose,
        native_call: call,
        authorized_at_unix_ms: now,
        expires_at_unix_ms: now + MAX_NATIVE_EXPORT_TTL_MS,
    };
    value.validate_projection(proof).unwrap();
    value
}

fn batch_audit(proofs: &[ReadProjectionProof]) -> SinkProjectionAudit {
    SinkProjectionAudit {
        destination: proofs[0].source_envelope.allowed_destinations[0].clone(),
        envelope_ids: proofs
            .iter()
            .map(|proof| proof.source_envelope.envelope_id.clone())
            .collect(),
        total_bytes: proofs.len() * 2,
    }
}

async fn seed_native_on(
    db: &crate::config::connection::DatabaseTransaction,
    proof: &ReadProjectionProof,
    physical: &str,
) -> AssistantDataExportAuthorization {
    seed_native_batch_on(db, std::slice::from_ref(proof), physical)
        .await
        .remove(0)
}

async fn seed_native_batch_on(
    db: &crate::config::connection::DatabaseTransaction,
    proofs: &[ReadProjectionProof],
    physical: &str,
) -> Vec<AssistantDataExportAuthorization> {
    let proof = &proofs[0];
    let sink = batch_audit(proofs);
    let row = crate::model_egress_store::SignalModelEgressStore::record_dispatch_intent_on(
        db,
        physical.into(),
        "export-1".into(),
        1,
        &sink,
        &proofs
            .iter()
            .map(|proof| proof.source_envelope.clone())
            .collect::<Vec<_>>(),
    )
    .await
    .unwrap();
    crate::model_egress_store::SignalModelEgressStore::link_context_dispatch_on(
        db,
        physical,
        &proof.root_conversation_id,
        false,
    )
    .await
    .unwrap();
    crate::model_egress_store::receipt_inputs(&row).unwrap();
    let call = NativeAssistantModelCall {
        physical_call_id: physical.into(),
        model_export_authorization_id: "export-1".into(),
        destination: sink.destination,
        authority_expires_at_unix_ms: None,
    };
    proofs
        .iter()
        .map(|proof| {
            grant(
                proof,
                call.clone(),
                row.authorized_at.timestamp_millis() as u64,
            )
        })
        .collect()
}

#[tokio::test]
async fn native_source_set_capacity_failure_rolls_back_receipt_consumption_uses_and_charges() {
    let store = setup().await;
    let mut policy = AssistantDataPolicy::default();
    store.settings(1, &policy).await.unwrap();
    let proof = source_proof("source-set-capacity");
    let txn = store.db.begin().await.unwrap();
    prepare_on(&txn, &proof, "{}", &policy, proof.created_at_unix_ms + 1)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    let before = crate::entity::agent_assistant_quota::Entity::find()
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let txn = store.db.begin().await.unwrap();
    let grant = seed_native_on(&txn, &proof, "physical-source-set-capacity").await;
    let seal =
        desk_diagnose_core::ai_assistant::native_export::AssistantNativeSourceSet::from_grants(
            std::slice::from_ref(&grant),
        )
        .unwrap();
    policy.max_metadata_bytes =
        before.metadata_bytes as u64 + serde_json::to_string(&seal).unwrap().len() as u64 - 1;
    assert!(matches!(
        super::super::native_export::record_call_on(&txn, &[grant], &policy).await,
        Err(AssistantStoreError::Capacity)
    ));
    txn.rollback().await.unwrap();
    assert_eq!(native::Entity::find().count(&store.db).await.unwrap(), 0);
    assert_eq!(
        authorization::Entity::find()
            .count(&store.db)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        source_use::Entity::find().count(&store.db).await.unwrap(),
        0
    );
    assert_eq!(
        memory_use::Entity::find().count(&store.db).await.unwrap(),
        0
    );
    assert!(
        load_projection_on(&store.db, "1", &proof.usage.projection_id)
            .await
            .unwrap()
            .unwrap()
            .row
            .first_consumed_at_ms
            .is_none()
    );
    assert_eq!(
        crate::entity::agent_assistant_quota::Entity::find()
            .one(&store.db)
            .await
            .unwrap()
            .unwrap(),
        before
    );
}

#[tokio::test]
async fn native_grant_use_rows_consumption_and_quota_commit_together_and_replay_once() {
    let store = setup().await;
    let policy = AssistantDataPolicy::default();
    store.settings(1, &policy).await.unwrap();
    let proof = source_proof("one");
    let txn = store.db.begin().await.unwrap();
    prepare_on(&txn, &proof, "{}", &policy, proof.created_at_unix_ms + 1)
        .await
        .unwrap();
    let grant = seed_native_on(&txn, &proof, "physical-1").await;
    let accepted = record_on(&txn, &grant, &policy).await.unwrap();
    let charged = quota::Entity::find()
        .filter(quota::Column::ActorId.eq("1"))
        .one(&txn)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(source_use::Entity::find().count(&txn).await.unwrap(), 1);
    assert_eq!(memory_use::Entity::find().count(&txn).await.unwrap(), 1);
    let projection = load_projection_on(&txn, "1", &proof.usage.projection_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        projection.row.first_consumed_at_ms,
        Some(grant.authorized_at_unix_ms as i64)
    );
    assert_eq!(
        projection.row.last_consumed_at_ms,
        projection.row.first_consumed_at_ms
    );
    let replay = record_on(&txn, &grant, &policy).await.unwrap();
    assert_eq!(replay.row, accepted.row);
    assert_eq!(
        quota::Entity::find()
            .filter(quota::Column::ActorId.eq("1"))
            .one(&txn)
            .await
            .unwrap()
            .unwrap(),
        charged
    );
    assert_eq!(authorization::Entity::find().count(&txn).await.unwrap(), 1);
    txn.commit().await.unwrap();
    assert!(
        load_on(&store.db, "2", &grant.authorization_id)
            .await
            .unwrap()
            .is_none()
    );
    let source = source_use::Entity::find()
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let memory = memory_use::Entity::find()
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    assert!(source.expires_at_ms.is_none() && memory.expires_at_ms.is_none());
    let use_bytes = metadata_bytes(&accepted.row).unwrap()
        + source_use_bytes(&source).unwrap()
        + memory_use_bytes(&memory).unwrap();
    assert!(use_bytes > accepted.row.proof_json.len() as i64);
}

#[tokio::test]
async fn capacity_failure_rolls_back_the_original_native_receipt_as_well_as_all_source_uses() {
    let store = setup().await;
    let mut policy = AssistantDataPolicy::default();
    store.settings(1, &policy).await.unwrap();
    let proof = source_proof("capacity");
    let txn = store.db.begin().await.unwrap();
    let prepared = prepare_on(&txn, &proof, "{}", &policy, proof.created_at_unix_ms + 1)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    let before = quota::Entity::find()
        .filter(quota::Column::ActorId.eq("1"))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    policy.max_metadata_bytes = before.metadata_bytes as u64;
    let txn = store.db.begin().await.unwrap();
    let grant = seed_native_on(&txn, &proof, "physical-capacity").await;
    assert!(matches!(
        record_on(&txn, &grant, &policy).await,
        Err(AssistantStoreError::Capacity)
    ));
    txn.rollback().await.unwrap();
    assert_eq!(native::Entity::find().count(&store.db).await.unwrap(), 0);
    assert_eq!(
        authorization::Entity::find()
            .count(&store.db)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        source_use::Entity::find().count(&store.db).await.unwrap(),
        0
    );
    assert_eq!(
        memory_use::Entity::find().count(&store.db).await.unwrap(),
        0
    );
    assert_eq!(
        load_projection_on(&store.db, "1", &proof.usage.projection_id)
            .await
            .unwrap()
            .unwrap()
            .row,
        prepared.row
    );
    assert_eq!(
        quota::Entity::find()
            .filter(quota::Column::ActorId.eq("1"))
            .one(&store.db)
            .await
            .unwrap()
            .unwrap(),
        before
    );
}

#[tokio::test]
async fn a_prepared_result_or_mismatched_native_audit_never_counts_as_an_actual_use() {
    let store = setup().await;
    let policy = AssistantDataPolicy::default();
    store.settings(1, &policy).await.unwrap();
    let proof = source_proof("audit");
    let txn = store.db.begin().await.unwrap();
    prepare_on(&txn, &proof, "{}", &policy, proof.created_at_unix_ms + 1)
        .await
        .unwrap();
    let accepted = seed_native_on(&txn, &proof, "physical-audit").await;
    for case in 0..4 {
        let mut changed = accepted.clone();
        match case {
            0 => changed.native_call.physical_call_id = "different-call".into(),
            1 => changed.native_call.model_export_authorization_id = "different-export".into(),
            2 => {
                changed.native_call.destination =
                    desk_agent_protocol::data_lineage::DestinationIdentity::Model {
                        connection_id: "another-account".into(),
                        connection_revision: 1,
                        model_id: "model".into(),
                        profile_revision: 1,
                    }
            }
            _ => changed.authorized_at_unix_ms += 1,
        }
        assert!(record_on(&txn, &changed, &policy).await.is_err());
    }
    assert_eq!(authorization::Entity::find().count(&txn).await.unwrap(), 0);
    assert!(
        load_projection_on(&txn, "1", &proof.usage.projection_id)
            .await
            .unwrap()
            .unwrap()
            .row
            .first_consumed_at_ms
            .is_none()
    );
    let mut unbound = proof.clone();
    unbound.usage.projection_id = native_ids::stable(Kind::AssistantReadProjection, &"not-stored");
    let missing = grant(
        &unbound,
        accepted.native_call,
        accepted.authorized_at_unix_ms,
    );
    assert!(record_on(&txn, &missing, &policy).await.is_err());
    txn.rollback().await.unwrap();
}

#[tokio::test]
async fn revoked_preparation_and_missing_reverse_uses_fail_closed_without_new_charges() {
    let store = setup().await;
    let policy = AssistantDataPolicy::default();
    store.settings(1, &policy).await.unwrap();
    let proof = source_proof("revoked");
    let txn = store.db.begin().await.unwrap();
    prepare_on(&txn, &proof, "{}", &policy, proof.created_at_unix_ms + 1)
        .await
        .unwrap();
    let grant = seed_native_on(&txn, &proof, "physical-revoked").await;
    record_on(&txn, &grant, &policy).await.unwrap();
    let before = quota::Entity::find()
        .filter(quota::Column::ActorId.eq("1"))
        .one(&txn)
        .await
        .unwrap()
        .unwrap();
    source_use::Entity::delete_many()
        .filter(source_use::Column::AuthorizationId.eq(&grant.authorization_id))
        .exec(&txn)
        .await
        .unwrap();
    assert!(record_on(&txn, &grant, &policy).await.is_err());
    assert_eq!(
        quota::Entity::find()
            .filter(quota::Column::ActorId.eq("1"))
            .one(&txn)
            .await
            .unwrap()
            .unwrap(),
        before
    );
    projection::Entity::update_many()
        .col_expr(
            projection::Column::RevokedAtMs,
            sea_orm::sea_query::Expr::value(Some(grant.authorized_at_unix_ms as i64)),
        )
        .filter(projection::Column::ProjectionId.eq(&proof.usage.projection_id))
        .exec(&txn)
        .await
        .unwrap();
    assert!(record_on(&txn, &grant, &policy).await.is_err());
    txn.rollback().await.unwrap();
}

mod batch;
