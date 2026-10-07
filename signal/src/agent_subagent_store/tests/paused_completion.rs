//! Original receipt delivery and child interpretation admission across goal pause.
use super::*;
use crate::agent_exec_store::SignalAgentExecStore;
use crate::agent_session_store::{EventAppend, SignalAgentSessionStore};
use desk_diagnose_core::{
    action_result::ActionResultOrigin,
    action_turn_fence::AssistantTurnFence,
    goal::GoalOwnerAction,
    seam::ClaimTurnParams,
    session::{ActionIdentity, ExecutionState, TriggerOrigin, TurnState, WorkKind},
};

#[tokio::test]
async fn original_receipt_backfills_paused_child_and_claim_waits_for_goal_resume() {
    let db = database().await;
    super::input_sources::add_input_tables(&db).await;
    super::creation::runnable_parent(&db).await;
    super::input_sources::release_parent(&db).await;
    super::input_sources::append_input(&db, true, "completion-goal").await;
    let store = SubAgentStore::new(db.clone());
    let task = super::input_sources::spawn_goal_child(&db, &store).await;
    super::input_sources::release_parent(&db).await;
    let run = load(&db, &task.task_id).await;
    let SubAgentClaimOutcome::Claimed(claimed) = store
        .claim_child(
            &super::creation::claim_params(&run),
            &task.task_id,
            run.fence(),
            &super::creation::destination(),
        )
        .await
        .unwrap()
    else {
        panic!("original child")
    };
    let mut child = claimed.session;
    child.policy_revision =
        desk_diagnose_core::assistant_policy::PERSONAL_ASSISTANT_POLICY_REVISION;
    let fence = AssistantTurnFence::from_session(&child).unwrap().unwrap();
    // Synthetic dispatch provenance is confined to this storage contract fixture.
    let origin = ActionResultOrigin {
        schema_version: 1,
        turn_fence: fence.clone(),
        tool_call_id: "call-paused-completion".into(),
        provider_id: "test.command".into(),
        tool_name: "exec_command".into(),
        source_object_id: "fixture-command".into(),
        source_envelope_ids: vec!["fixture-original-input".into()],
        sensitivity: desk_agent_protocol::data_lineage::Sensitivity::Sensitive,
        retention: desk_agent_protocol::data_lineage::RetentionBoundary {
            expires_at_unix_ms: Some((chrono::Utc::now().timestamp_millis() + 120_000) as u64),
            delete_with_run: true,
        },
        ephemeral: false,
        command_completion: None,
    };
    origin.validate().unwrap();
    let work =
        super::native_cancel::seed_native(&db, &child.conversation_id, "paused-completion").await;
    seed_origin(&db, &work, &origin).await;
    child.execution_state = ExecutionState::Executing {
        action: ActionIdentity::agent_exec(
            work.id,
            "action-paused-completion",
            "paused-completion",
        ),
    };
    child.finish_turn(TurnState::Idle, chrono::Utc::now().to_rfc3339());
    save_child_session(&db, &mut child).await.unwrap();
    let goal = super::input_sources::goal(&db).await;
    let paused = super::paused_permission::control(&db, &goal, GoalOwnerAction::Pause)
        .await
        .unwrap();
    let outcome = desk_agent_protocol::AgentOutcome::Err(desk_agent_protocol::AgentError {
        kind: desk_agent_protocol::AgentErrorKind::Cancelled,
        message: "fixture original worker result".into(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    });
    let native = SignalAgentExecStore::new(db.clone());
    let work = native
        .finalize(
            "original-host",
            "paused-completion",
            &desk_agent_protocol::edge_exec::EdgeExecDisposition::Executed { outcome },
        )
        .await
        .unwrap()
        .unwrap();
    let (output, receipt) = native.command_result(&work).await.unwrap().unwrap();
    let (_, replayed_receipt) = native.command_result(&work).await.unwrap().unwrap();
    assert_eq!(replayed_receipt, receipt);
    let mut altered = work.clone();
    altered.result_text = Some("altered output".into());
    assert!(native.command_result(&altered).await.is_err());
    let event = work.event_id.clone();
    let sessions = SignalAgentSessionStore::new(db.clone());
    let now = chrono::Utc::now().to_rfc3339();
    for expected in [EventAppend::Appended, EventAppend::AlreadyPresent] {
        assert_eq!(
            sessions
                .deliver_work_completion_with_envelope(
                    &child.conversation_id,
                    work.id,
                    WorkKind::AgentExec,
                    &event,
                    "paused-completion",
                    &origin.tool_call_id,
                    "action-paused-completion",
                    &output.content,
                    Some(receipt.envelope.clone()),
                    output.format,
                    &now,
                )
                .await
                .unwrap(),
            expected
        );
    }
    let delivered = super::paused_permission::session(&db, &child.conversation_id).await;
    assert_eq!(delivered.pending_auto_triggers.len(), 1);
    assert_eq!(delivered.pending_auto_triggers[0].event_id, event);
    assert!(!delivered.turn_state.is_active());
    assert!(!delivered.execution_state.contains(&receipt.action));
    let messages: Vec<_> = delivered
        .conversation
        .iter()
        .filter(|m| m.message_id == event)
        .collect();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].data_envelope.as_ref(), Some(&receipt.envelope));
    let params = ClaimTurnParams {
        conversation_id: child.conversation_id.clone(),
        actor_id: child.actor_id.clone(),
        device_id: child.device_id.clone(),
        policy_revision: child.policy_revision,
        current_pdp_scope: child.scope_snapshot.clone(),
        turn_id: "completion-interpretation".into(),
        request_id: None,
        connection_id: None,
        trigger_origin: TriggerOrigin::ExecCompletion,
        now: chrono::Utc::now().to_rfc3339(),
    };
    assert!(
        store
            .claim_child_completion(&params, &super::creation::destination(), &event)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        super::paused_permission::session(&db, &child.conversation_id).await,
        delivered
    );
    super::paused_permission::control(&db, &paused, GoalOwnerAction::Resume)
        .await
        .unwrap();
    assert!(
        store
            .claim_child_completion(&params, &super::creation::destination(), "wrong-event")
            .await
            .unwrap()
            .is_none()
    );
    let restored = super::paused_permission::session(&db, &child.conversation_id).await;
    let claimed = store
        .claim_child_completion(&params, &super::creation::destination(), &event)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.agent_role, restored.agent_role);
    assert_eq!(claimed.conversation, restored.conversation);
    assert!(claimed.turn_state.is_active());
    assert!(
        store
            .claim_child_completion(&params, &super::creation::destination(), &event)
            .await
            .unwrap()
            .is_none()
    );
}

async fn seed_origin(
    db: &DatabaseConnection,
    work: &crate::entity::agent_exec_task::Model,
    origin: &ActionResultOrigin,
) {
    use crate::{
        capability_grant_store::CapabilityDispatchPayload,
        entity::agent_capability_dispatch_outbox as outbox,
    };
    use desk_agent_protocol::{
        capability_grant::CapabilityRiskTier, capability_provider::CapabilityEffect,
    };
    use desk_diagnose_core::provider_preflight::ObservedCapabilityAuthority;
    db.execute(
        Schema::new(db.get_database_backend())
            .create_table_from_entity(outbox::Entity)
            .if_not_exists(),
    )
    .await
    .unwrap();
    // Frozen transport metadata only: this fixture issues no grant or send permit.
    let payload = CapabilityDispatchPayload {
        observed_authority: ObservedCapabilityAuthority {
            target_session_id: None,
            envelope_ids: vec!["fixture-original-input".into()],
            content_digests_sha256: vec![],
            provider_id: origin.provider_id.clone(),
            capability_id: "fixture.command".into(),
            tool_name: origin.tool_name.clone(),
            tool_schema_version: 1,
            effect: CapabilityEffect::ExecuteCommand,
            risk_tier: CapabilityRiskTier::R3,
            canonical_input_sha256: "a".repeat(64),
            resources: vec!["device:1".into()],
            operations: vec!["execute".into()],
            export_destinations: vec![],
        },
        command_origin: Some(origin.clone()),
        command_receipt: None,
        command_export: None,
        dispatch_id: work.execution_generation.clone(),
        grant_id: "fixture-grant".into(),
        reservation_id: "fixture-reservation".into(),
        work_id: work.id,
        call_id: work.exec_request_id.clone(),
        generation: 1,
        input_revision: 1,
        input_watermark: 1,
        canonical_input_json: "{}".into(),
        canonical_input_digest_sha256: "a".repeat(64),
        provider_id: origin.provider_id.clone(),
        capability_id: "fixture.command".into(),
        tool_name: origin.tool_name.clone(),
    };
    outbox::ActiveModel {
        dispatch_id: Set(work.execution_generation.clone()),
        call_id: Set(work.exec_request_id.clone()),
        work_id: Set(work.id),
        reservation_id: Set("fixture-reservation".into()),
        generation: Set(1),
        state: Set("claimed".into()),
        payload_json: Set(serde_json::to_string(&payload).unwrap()),
        payload_schema_version: Set(1),
        created_at: Set(chrono::Utc::now()),
        updated_at: Set(chrono::Utc::now()),
        ..Default::default()
    }
    .insert(db)
    .await
    .unwrap();
}
