use super::*;
use crate::capability_grant_store::{self, CapabilityDispatchPayload, PreparedCapabilityPayload};
use crate::entity::{
    agent_action_item as work, agent_capability_dispatch_outbox as outbox,
    agent_grant_reservation as reservation,
};
use desk_agent_protocol::{
    capability_grant::CapabilityRiskTier,
    capability_provider::CapabilityEffect,
    data_lineage::{RetentionBoundary, Sensitivity},
};
use sea_orm::{ActiveModelTrait, Set};
use sha2::{Digest, Sha256};

async fn fixture() -> (DatabaseConnection, i64) {
    let db = crate::config::test_support::Database::connect("sqlite::memory:")
        .await
        .unwrap();
    crate::db::initialize_schema(&db).await.unwrap();
    let now = chrono::Utc::now();
    let tool = desk_diagnose_core::command_confirmation::COMMAND_TOOL;
    let origin = desk_diagnose_core::action_result::ActionResultOrigin {
        schema_version: 1,
        command_completion: None,
        turn_fence: desk_diagnose_core::action_turn_fence::AssistantTurnFence {
            schema_version: 1,
            conversation_id: "child".into(),
            turn_id: "turn".into(),
            actor_id: "1".into(),
            device_id: "original-device".into(),
            input_revision: 1,
            lease_token: 1,
            control_revision: 1,
            delegation: Some(
                desk_diagnose_core::action_turn_fence::DelegatedActionFence {
                    root_conversation_id: "root".into(),
                    group_id: "group".into(),
                    task_id: "task".into(),
                    source_epoch: 1,
                },
            ),
        },
        tool_call_id: "model-call".into(),
        provider_id: "provider".into(),
        tool_name: tool.into(),
        source_object_id: "source".into(),
        source_envelope_ids: vec!["evidence".into()],
        sensitivity: Sensitivity::Sensitive,
        retention: RetentionBoundary {
            expires_at_unix_ms: None,
            delete_with_run: true,
        },
        ephemeral: false,
    };
    let authority = desk_diagnose_core::provider_preflight::ObservedCapabilityAuthority {
        target_session_id: None,
        envelope_ids: vec!["evidence".into()],
        content_digests_sha256: vec![],
        provider_id: "provider".into(),
        capability_id: "command".into(),
        tool_name: tool.into(),
        tool_schema_version: 1,
        effect: CapabilityEffect::ExecuteCommand,
        risk_tier: CapabilityRiskTier::R2,
        canonical_input_sha256: format!("{:x}", Sha256::digest(b"{}")),
        resources: vec![],
        operations: vec![],
        export_destinations: vec![],
    };
    let prepared = PreparedCapabilityPayload {
        observed_authority: authority.clone(),
        grant_id: "grant".into(),
        reservation_id: "reservation".into(),
        call_id: "exec-request".into(),
        generation: 1,
        input_revision: 1,
        input_watermark: 1,
        canonical_input_json: "{}".into(),
        canonical_input_digest_sha256: authority.canonical_input_sha256.clone(),
        provider_id: "provider".into(),
        capability_id: "command".into(),
        tool_name: tool.into(),
    };
    let generation = "capability:exec-request:1";
    let work = work::ActiveModel {
        kind: Set(capability_grant_store::CAPABILITY_WORK_KIND.into()),
        action_request_id: Set("exec-request".into()),
        conversation_id: Set("child".into()),
        turn_id: Set("turn".into()),
        tool_call_id: Set("exec-request".into()),
        actor_id: Set("1".into()),
        target_device_id: Set("original-device".into()),
        status: Set(capability_grant_store::CAPABILITY_WORK_DISPATCHING.into()),
        attempt: Set(1),
        execution_id: Set(Some(generation.into())),
        dispatched_attempt: Set(Some(1)),
        dispatch_intent_at: Set(Some(now)),
        draft_hash: Set(prepared.canonical_input_digest_sha256.clone()),
        policy_revision: Set(1),
        is_side_effecting: Set(true),
        payload_json: Set(serde_json::to_string(&prepared).unwrap()),
        payload_schema_version: Set(1),
        completion_event_id: Set("completion".into()),
        completion_delivery_state: Set("pending".into()),
        created_at: Set(now),
        updated_at: Set(now),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();
    let payload = CapabilityDispatchPayload {
        observed_authority: authority,
        command_origin: Some(origin),
        command_receipt: None,
        command_export: None,
        dispatch_id: generation.into(),
        grant_id: prepared.grant_id.clone(),
        reservation_id: prepared.reservation_id.clone(),
        work_id: work.id,
        call_id: prepared.call_id.clone(),
        generation: prepared.generation,
        input_revision: prepared.input_revision,
        input_watermark: prepared.input_watermark,
        canonical_input_json: prepared.canonical_input_json.clone(),
        canonical_input_digest_sha256: prepared.canonical_input_digest_sha256.clone(),
        provider_id: prepared.provider_id.clone(),
        capability_id: prepared.capability_id.clone(),
        tool_name: prepared.tool_name.clone(),
    };
    outbox::ActiveModel {
        dispatch_id: Set(generation.into()),
        call_id: Set(prepared.call_id.clone()),
        work_id: Set(work.id),
        reservation_id: Set(prepared.reservation_id.clone()),
        generation: Set(1),
        state: Set("sending".into()),
        payload_json: Set(serde_json::to_string(&payload).unwrap()),
        payload_schema_version: Set(1),
        created_at: Set(now),
        updated_at: Set(now),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();
    reservation::ActiveModel {
        reservation_id: Set(prepared.reservation_id.clone()),
        grant_id: Set(prepared.grant_id),
        run_id: Set("child".into()),
        call_id: Set(prepared.call_id),
        work_id: Set(work.id),
        canonical_input_digest_sha256: Set(prepared.canonical_input_digest_sha256),
        state: Set(capability_grant_store::RESERVATION_STATUS_COMMITTED.into()),
        generation: Set(1),
        created_at: Set(now),
        updated_at: Set(now),
        ..Default::default()
    }
    .insert(&db)
    .await
    .unwrap();
    let row = crate::agent_exec_store::SignalAgentExecStore::new(db.clone())
        .create(
            "exec-request",
            generation,
            "child",
            "model-call",
            "original-host",
            now + chrono::Duration::minutes(1),
        )
        .await
        .unwrap();
    let mut active: task::ActiveModel = row.into();
    active.cancel_requested_at = Set(Some(now));
    active.cancel_requested_by = Set(Some("1".into()));
    let row = active.update(&db).await.unwrap();
    (db, row.id)
}

#[tokio::test]
async fn cancelled_child_native_stop_uses_frozen_origin_without_current_parent_session() {
    let (db, id) = fixture().await;
    let before = task::Entity::find_by_id(id)
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let selected = candidate(&db, id).await.unwrap().unwrap();
    assert_eq!(selected.target_connection_id, "original-host");
    assert_eq!(selected.audience, "original-device");
    assert_eq!(
        selected
            .origin
            .turn_fence
            .delegation
            .as_ref()
            .unwrap()
            .task_id,
        "task"
    );
    assert_eq!(
        task::Entity::find_by_id(id)
            .one(&db)
            .await
            .unwrap()
            .unwrap(),
        before
    );
    let dispatcher =
        SignalCommandCancelDispatcher::new(db.clone(), Arc::new(SharedConnectionMap::default()));
    assert_eq!(dispatcher.scan_once(0).await.unwrap(), (None, 0));
    assert_eq!(
        task::Entity::find_by_id(id)
            .one(&db)
            .await
            .unwrap()
            .unwrap(),
        before
    );
}

#[tokio::test]
async fn stop_rejects_changed_requester_and_dispatched_origin() {
    let (db, id) = fixture().await;
    let original = task::Entity::find_by_id(id)
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let mut wrong: task::ActiveModel = original.clone().into();
    wrong.cancel_requested_by = Set(Some("different-owner".into()));
    wrong.update(&db).await.unwrap();
    assert!(candidate(&db, id).await.is_err());
    let mut restored: task::ActiveModel = original.into();
    restored.cancel_requested_by = Set(Some("1".into()));
    restored.tool_call_id = Set("replacement-call".into());
    restored.update(&db).await.unwrap();
    assert!(candidate(&db, id).await.is_err());
}

#[tokio::test]
async fn desired_cancel_and_unknown_receipt_do_not_prevent_late_actual_result() {
    let (db, id) = fixture().await;
    let before = task::Entity::find_by_id(id)
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let store = crate::agent_exec_store::SignalAgentExecStore::new(db.clone());
    let unknown = EdgeExecDisposition::ExecutionStateUnknown {
        reason: "original worker receipt delayed".into(),
    };
    let uncertain = store
        .finalize("original-host", &before.execution_generation, &unknown)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(uncertain.status, "unknown");
    assert!(candidate(&db, id).await.unwrap().is_some());
    store.consume_event(&before.event_id).await.unwrap();
    let actual = EdgeExecDisposition::Executed {
        outcome: AgentOutcome::Err(safe(
            AgentErrorKind::Cancelled,
            "original process tree stopped",
        )),
    };
    assert!(
        store
            .finalize("replacement-host", &before.execution_generation, &actual)
            .await
            .unwrap()
            .is_none()
    );
    let done = store
        .finalize("original-host", &before.execution_generation, &actual)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(done.status, "done");
    assert_eq!(
        done.delivery_state,
        crate::agent_exec_store::DELIVERY_PENDING
    );
    assert_eq!(done.cancel_requested_at, before.cancel_requested_at);
    assert_eq!(done.deadline, before.deadline);
    assert!(candidate(&db, id).await.unwrap().is_none());
    assert_eq!(
        store
            .finalize("original-host", &before.execution_generation, &unknown)
            .await
            .unwrap()
            .unwrap(),
        done
    );
}
