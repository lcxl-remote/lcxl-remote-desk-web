use super::*;
use crate::{
    capability_grant_store::*,
    entity::{agent_action_item as work, agent_capability_dispatch_outbox as outbox},
};
use desk_agent_protocol::capability_provider::CapabilityEffect;
use desk_diagnose_core::{
    provider_preflight::ObservedCapabilityAuthority, session::TurnState,
    subagent::state::CompletionDisposition,
};
use sea_orm::{ActiveModelTrait, IntoActiveModel};
use serde_json::json;
use sha2::{Digest, Sha256};

async fn read_work(
    db: &DatabaseConnection,
    child: &PersistedAgentSession,
    outcome: CapabilityDispatchOutcome,
) -> (work::Model, outbox::Model) {
    let now = chrono::Utc::now();
    let digest = format!("{:x}", Sha256::digest(b"{}"));
    let authority = ObservedCapabilityAuthority {
        target_session_id: None,
        envelope_ids: vec![],
        content_digests_sha256: vec![],
        provider_id: "system.info".into(),
        capability_id: "system.info.read".into(),
        tool_name: "read_system_info".into(),
        tool_schema_version: 1,
        effect: CapabilityEffect::ReadDevice,
        risk_tier: desk_agent_protocol::capability_grant::CapabilityRiskTier::R0,
        canonical_input_sha256: digest.clone(),
        resources: vec!["target:current_device".into()],
        operations: vec!["observe".into()],
        export_destinations: vec![],
    };
    let prepared = PreparedCapabilityPayload {
        observed_authority: authority,
        grant_id: "grant".into(),
        reservation_id: "reservation".into(),
        call_id: "read-call".into(),
        generation: 1,
        input_revision: child.input_revision,
        input_watermark: child.latest_input_seq,
        canonical_input_json: "{}".into(),
        canonical_input_digest_sha256: digest.clone(),
        provider_id: "system.info".into(),
        capability_id: "system.info.read".into(),
        tool_name: "read_system_info".into(),
    };
    let completion = CapabilityDispatchCompletion {
        dispatch_id: "dispatch".into(),
        call_id: "read-call".into(),
        generation: 1,
        outcome,
        result_digest_sha256: "a".repeat(64),
    };
    let status = match outcome {
        CapabilityDispatchOutcome::Succeeded => CAPABILITY_WORK_SUCCEEDED,
        CapabilityDispatchOutcome::Failed => CAPABILITY_WORK_FAILED,
    };
    let work: work::Model = serde_json::from_value(json!({"id":1,"kind":CAPABILITY_WORK_KIND,"action_request_id":"read-call","exec_request_id":null,"conversation_id":child.conversation_id,"turn_id":child.current_turn_id,"tool_call_id":"read-call","actor_id":child.actor_id,"target_device_id":child.device_id,"status":status,"owner_node":null,"claim_token":null,"attempt":1,"lease_expires_at":null,"execution_id":"capability:read-call:1","dispatched_attempt":1,"dispatch_intent_at":now,"approval_id":null,"approval_expires_at":null,"approved_at":null,"draft_hash":digest,"policy_revision":1,"is_side_effecting":false,"payload_json":serde_json::to_string(&prepared).unwrap(),"payload_schema_version":1,"result_json":serde_json::to_string(&completion).unwrap(),"result_schema_version":1,"resolution":null,"manual_resolved_at":null,"cancel_requested_at":null,"cancel_requested_by":null,"cancel_generation":null,"completion_event_id":"completion","completion_delivery_state":"delivered","created_at":now,"updated_at":now})).unwrap();
    let payload = CapabilityDispatchPayload {
        observed_authority: prepared.observed_authority.clone(),
        command_origin: None,
        command_receipt: None,
        command_export: None,
        dispatch_id: "dispatch".into(),
        grant_id: prepared.grant_id.clone(),
        reservation_id: prepared.reservation_id.clone(),
        work_id: work.id,
        call_id: prepared.call_id.clone(),
        generation: 1,
        input_revision: prepared.input_revision,
        input_watermark: prepared.input_watermark,
        canonical_input_json: prepared.canonical_input_json.clone(),
        canonical_input_digest_sha256: prepared.canonical_input_digest_sha256.clone(),
        provider_id: prepared.provider_id.clone(),
        capability_id: prepared.capability_id.clone(),
        tool_name: prepared.tool_name.clone(),
    };
    let dispatch = outbox::Model {
        id: 1,
        dispatch_id: "dispatch".into(),
        call_id: "read-call".into(),
        work_id: work.id,
        reservation_id: "reservation".into(),
        generation: 1,
        state: DISPATCH_OUTBOX_COMPLETED.into(),
        payload_json: serde_json::to_string(&payload).unwrap(),
        payload_schema_version: 1,
        computer_binding_json: None,
        computer_acceptance_json: None,
        computer_background_json: None,
        computer_cancel_json: None,
        created_at: now,
        updated_at: now,
    };
    work.clone().into_active_model().insert(db).await.unwrap();
    dispatch
        .clone()
        .into_active_model()
        .insert(db)
        .await
        .unwrap();
    (work, dispatch)
}

#[tokio::test]
async fn ordinary_read_success_and_failure_close_the_lifecycle_and_settle_once() {
    for outcome in [
        CapabilityDispatchOutcome::Succeeded,
        CapabilityDispatchOutcome::Failed,
    ] {
        let db = database().await;
        let mut child = super::lifecycle::child(&db).await;
        let task = load(&db, &child.agent_role.binding().unwrap().task_id).await;
        read_work(&db, &child, outcome).await;
        let txn = db.begin().await.unwrap();
        let facts = super::super::facts::runtime_facts_on(
            &txn,
            &child,
            chrono::Utc::now().timestamp_millis(),
        )
        .await
        .unwrap();
        assert!(facts.dependencies.is_empty());
        assert!(facts.completion.required_receipts.is_empty());
        txn.commit().await.unwrap();
        let report = super::lifecycle::report();
        super::lifecycle::append_report(&db, &mut child, &report).await;
        child.finish_turn(TurnState::Idle, chrono::Utc::now().to_rfc3339());
        let store = SubAgentStore::new(db.clone());
        assert_eq!(
            store
                .settle_answer_for_turn(&mut child, task.fence(), report.summary.clone())
                .await
                .unwrap(),
            Ok(CompletionDisposition::Complete)
        );
        assert_eq!(
            load(&db, &task.binding.task_id).await.state,
            SubAgentState::Completed
        );
        assert_eq!(
            crate::entity::agent_subagent_inbox::Entity::find()
                .filter(crate::entity::agent_subagent_inbox::Column::EventKind.eq("completed"))
                .count(&db)
                .await
                .unwrap(),
            1
        );
        assert_eq!(work::Entity::find().count(&db).await.unwrap(), 1);
    }
}

#[tokio::test]
async fn terminal_read_requires_consistent_original_completion_and_unknown_work_still_waits() {
    for case in 0..5 {
        let db = database().await;
        let child = super::lifecycle::child(&db).await;
        let (work, dispatch) = read_work(&db, &child, CapabilityDispatchOutcome::Succeeded).await;
        match case {
            0 => {
                work::Entity::update_many()
                    .set(work::ActiveModel {
                        result_json: Set(None),
                        ..Default::default()
                    })
                    .exec(&db)
                    .await
                    .unwrap();
            }
            1 => {
                outbox::Entity::update_many()
                    .set(outbox::ActiveModel {
                        generation: Set(2),
                        ..Default::default()
                    })
                    .exec(&db)
                    .await
                    .unwrap();
            }
            2 => {
                work::Entity::update_many()
                    .set(work::ActiveModel {
                        is_side_effecting: Set(true),
                        ..Default::default()
                    })
                    .exec(&db)
                    .await
                    .unwrap();
            }
            3 => {
                outbox::Entity::update_many()
                    .set(outbox::ActiveModel {
                        state: Set(DISPATCH_OUTBOX_OUTCOME_UNKNOWN.into()),
                        ..Default::default()
                    })
                    .exec(&db)
                    .await
                    .unwrap();
                work::Entity::update_many()
                    .set(work::ActiveModel {
                        status: Set(CAPABILITY_WORK_OUTCOME_UNKNOWN.into()),
                        result_json: Set(None),
                        result_schema_version: Set(None),
                        ..Default::default()
                    })
                    .exec(&db)
                    .await
                    .unwrap();
            }
            4 => {
                let mut prepared: PreparedCapabilityPayload =
                    serde_json::from_str(&work.payload_json).unwrap();
                let mut payload: CapabilityDispatchPayload =
                    serde_json::from_str(&dispatch.payload_json).unwrap();
                prepared.observed_authority.effect = CapabilityEffect::MutateApplication;
                payload.observed_authority.effect = CapabilityEffect::MutateApplication;
                work::Entity::update_many()
                    .set(work::ActiveModel {
                        is_side_effecting: Set(true),
                        payload_json: Set(serde_json::to_string(&prepared).unwrap()),
                        ..Default::default()
                    })
                    .exec(&db)
                    .await
                    .unwrap();
                outbox::Entity::update_many()
                    .set(outbox::ActiveModel {
                        payload_json: Set(serde_json::to_string(&payload).unwrap()),
                        ..Default::default()
                    })
                    .exec(&db)
                    .await
                    .unwrap();
            }
            _ => unreachable!(),
        }
        let txn = db.begin().await.unwrap();
        let facts = super::super::facts::runtime_facts_on(
            &txn,
            &child,
            chrono::Utc::now().timestamp_millis(),
        )
        .await;
        if case < 3 {
            assert!(facts.is_err());
        } else {
            assert_eq!(facts.unwrap().dependencies.len(), 1);
        }
        txn.rollback().await.unwrap();
    }
}

#[tokio::test]
async fn completed_read_recovers_old_wait_to_queued_then_normal_answer_without_redispatch() {
    let db = database().await;
    let mut child = super::lifecycle::child(&db).await;
    let task_id = child.agent_role.binding().unwrap().task_id.clone();
    read_work(&db, &child, CapabilityDispatchOutcome::Succeeded).await;
    let now = chrono::Utc::now();
    child.finish_turn(TurnState::Idle, now.to_rfc3339());
    save_child_session(&db, &mut child).await.unwrap();
    let mut run = load(&db, &task_id).await;
    run.set_dependencies(
        vec![desk_diagnose_core::subagent::state::TaskDependency::Work {
            work_id: "work:1".into(),
        }],
        &now.to_rfc3339(),
    )
    .unwrap();
    let row = run_row::Entity::find()
        .filter(run_row::Column::TaskId.eq(&task_id))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    replace_run_on(&db, &row, &run, now.timestamp_millis())
        .await
        .unwrap();
    synchronize_control_on(&db, &run, now.timestamp_millis(), false)
        .await
        .unwrap();
    let store = SubAgentStore::new(db.clone());
    assert!(store.reconcile_task(&task_id).await.unwrap());
    let queued = load(&db, &task_id).await;
    assert_eq!(queued.state, SubAgentState::Queued);
    assert!(queued.terminal_report.is_none());
    let SubAgentClaimOutcome::Claimed(claimed) = store
        .claim_child(
            &super::creation::claim_params(&queued),
            &task_id,
            queued.fence(),
            &super::creation::destination(),
        )
        .await
        .unwrap()
    else {
        panic!("normal finite continuation")
    };
    let mut child = claimed.session;
    let report = super::lifecycle::report();
    super::lifecycle::append_report(&db, &mut child, &report).await;
    child.finish_turn(TurnState::Idle, chrono::Utc::now().to_rfc3339());
    assert_eq!(
        store
            .settle_answer_for_turn(&mut child, queued.fence(), report.summary)
            .await
            .unwrap(),
        Ok(CompletionDisposition::Complete)
    );
    assert_eq!(load(&db, &task_id).await.state, SubAgentState::Completed);
    assert!(!store.reconcile_task(&task_id).await.unwrap());
    assert_eq!(work::Entity::find().count(&db).await.unwrap(), 1);
    assert_eq!(outbox::Entity::find().count(&db).await.unwrap(), 1);
}
