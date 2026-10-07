//! A published exact command, dispatched once and recovered from original facts.
use super::*;
use crate::capability_grant_store::{
    DispatchIntentResult, PrepareCapabilityCall, SignalCapabilityGrantStore,
};
use crate::entity::{
    agent_action_item, agent_capability_dispatch_outbox, agent_capability_grant, agent_exec_task,
    agent_grant_reservation,
};
use desk_agent_protocol::{
    capability_grant::CapabilityRiskTier, capability_provider::ProductSurface,
    edge_exec::EdgeExecDisposition,
};
use desk_diagnose_core::{
    action_result::ActionResultOrigin,
    capability_grant::CapabilityGrantCall,
    session::{ExecutionState, TurnState},
    subagent::tools::{self, Operation},
};
use sea_orm::Schema;
use sha2::{Digest, Sha256};

async fn fixture() -> (
    DatabaseConnection,
    ScheduleStore,
    SubAgentStore,
    PersistedAgentSession,
    String,
    String,
) {
    let (db, schedule, store, mut parent, _) = published_parent_with_command(true).await;
    let schema = Schema::new(db.get_database_backend());
    for mut table in [
        schema.create_table_from_entity(agent_capability_grant::Entity),
        schema.create_table_from_entity(agent_grant_reservation::Entity),
    ] {
        db.execute(table.if_not_exists()).await.unwrap();
    }
    let request = super::super::creation::spawn_request();
    let spawn = super::super::main_tools::committed_call(
        &db,
        &mut parent,
        "command-child",
        tools::SPAWN,
        serde_json::to_value(&request).unwrap(),
    )
    .await;
    store
        .execute_main_tool(
            &mut parent,
            &spawn,
            Operation::Spawn(request),
            "command-child-result",
        )
        .await
        .unwrap();
    let task_id = run_row::Entity::find()
        .one(&db)
        .await
        .unwrap()
        .unwrap()
        .task_id;
    let canonical = command_fixture_input();
    let call = super::super::main_tools::committed_call(
        &db,
        &mut parent,
        "original-command",
        desk_diagnose_core::command_confirmation::COMMAND_TOOL,
        serde_json::from_str(&canonical).unwrap(),
    )
    .await;
    let registry = desk_diagnose_core::ai_assistant::ai_assistant_provider_registry();
    let capability = registry.capability_for_tool(&call.name).unwrap();
    let provider = registry
        .provider_for_capability(&capability.wire.capability_id)
        .unwrap();
    let mut origin = ActionResultOrigin::capture(&registry, &parent, &call).unwrap();
    let now = chrono::Utc::now().timestamp_millis() as u64;
    let context = desk_diagnose_core::command_completion::CommandCompletionContext::capture(
        &parent,
        super::super::creation::destination(),
        now,
        60000,
    )
    .unwrap();
    origin.retention.expires_at_unix_ms = Some(context.expires_at_unix_ms);
    origin.command_completion = Some(context);
    let key = format!(
        "{}:{}:{}",
        parent.conversation_id,
        parent.current_turn_id.as_deref().unwrap(),
        call.id
    );
    let call_id = format!("capability-call-{:x}", Sha256::digest(key.as_bytes()));
    let grant_id =
        crate::capability_grant_store::task_grant::identity(&parent.conversation_id, &call_id);
    let digest = format!("{:x}", Sha256::digest(canonical.as_bytes()));
    let resources = vec!["device:1".into()];
    let operations = vec!["execute".into()];
    let command_store = SignalCapabilityGrantStore::new(db.clone());
    let prepared = || PrepareCapabilityCall {
        grant_id: &grant_id,
        call_id: &call_id,
        turn_id: parent.current_turn_id.as_deref().unwrap(),
        input_revision: parent.input_revision,
        input_watermark: parent.latest_input_seq,
        generation: 1,
        canonical_input_json: &canonical,
        call: CapabilityGrantCall {
            actor_id: &parent.actor_id,
            run_id: &parent.conversation_id,
            input_revision: parent.input_revision,
            surface: ProductSurface::OssPersonalOwner,
            target_device_id: &parent.device_id,
            target_session_id: None,
            provider_id: &provider.wire.provider_id,
            capability_id: &capability.wire.capability_id,
            tool_name: &call.name,
            tool_schema_version: capability.wire.input_schema_version,
            effect: capability.wire.effect,
            risk_tier: CapabilityRiskTier::R3,
            resource_scope: &resources,
            operation_scope: &operations,
            export_destinations: &[],
            envelope_ids: &[],
            content_digests_sha256: &[],
            canonical_input_digest_sha256: &digest,
            byte_count: canonical.len() as u64,
            item_count: 1,
            policy_revision: parent.policy_revision,
            readiness_revision: 1,
            now_unix_ms: now,
        },
    };
    command_store.prepare(prepared()).await.unwrap();
    let DispatchIntentResult::Recorded { dispatch_id, .. } = command_store
        .record_dispatch_intent(prepared())
        .await
        .unwrap()
    else {
        panic!("original dispatch intent required");
    };
    command_store
        .claim_command_dispatch(
            &dispatch_id,
            now,
            "original-host",
            &parent.conversation_id,
            &call.id,
            60000,
            &origin,
            None,
        )
        .await
        .unwrap();
    (db, schedule, store, parent, task_id, dispatch_id)
}

async fn original_run(db: &DatabaseConnection, root: &str) -> occurrence::Model {
    occurrence::Entity::find()
        .filter(occurrence::Column::RunId.eq(root))
        .one(db)
        .await
        .unwrap()
        .unwrap()
}

async fn saved_root(db: &DatabaseConnection, root: &str) -> PersistedAgentSession {
    let row = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq(root))
        .one(db)
        .await
        .unwrap()
        .unwrap();
    PersistedAgentSession::decode_json(&row.state_json).unwrap()
}

fn known_result() -> EdgeExecDisposition {
    EdgeExecDisposition::Executed {
        outcome: desk_agent_protocol::AgentOutcome::Err(desk_agent_protocol::AgentError {
            kind: desk_agent_protocol::AgentErrorKind::Internal,
            message: "The original command failed".into(),
            retryable: false,
            safe_for_model: true,
            error_code: None,
        }),
    }
}

async fn expire_as_unknown(
    db: &DatabaseConnection,
    schedule: &ScheduleStore,
    parent: &PersistedAgentSession,
    generation: &str,
) {
    let exec = crate::agent_exec_store::SignalAgentExecStore::new(db.clone());
    let native = exec
        .finalize(
            "original-host",
            generation,
            &EdgeExecDisposition::ExecutionStateUnknown {
                reason: "Original worker result is unavailable".into(),
            },
        )
        .await
        .unwrap()
        .unwrap();
    SignalCapabilityGrantStore::new(db.clone())
        .mark_dispatch_outcome_unknown(
            generation,
            &native.exec_request_id,
            1,
            chrono::Utc::now().timestamp_millis() as u64,
        )
        .await
        .unwrap();
    let expired = chrono::Utc::now() - chrono::Duration::seconds(1);
    occurrence::Entity::update_many()
        .set(occurrence::ActiveModel {
            lease_deadline: Set(Some(expired.timestamp_millis())),
            cancel_requested_at: Set(Some(expired.timestamp_millis())),
            ..Default::default()
        })
        .filter(occurrence::Column::RunId.eq(&parent.conversation_id))
        .exec(db)
        .await
        .unwrap();
    session_row::Entity::update_many()
        .set(session_row::ActiveModel {
            lease_deadline: Set(Some(expired)),
            ..Default::default()
        })
        .filter(session_row::Column::ConversationId.eq(&parent.conversation_id))
        .exec(db)
        .await
        .unwrap();
    assert!(
        schedule
            .recover_action_free_fresh_task(&parent.conversation_id)
            .await
            .unwrap()
    );
    SubAgentStore::new(db.clone())
        .close_scheduled_source(&parent.conversation_id, &parent.actor_id, &parent.device_id)
        .await
        .unwrap();
    let work = original_run(db, &parent.conversation_id).await;
    assert_eq!(work.status, "outcome_unknown");
    assert!(
        work.result_ref
            .as_deref()
            .unwrap()
            .starts_with("delegated-effects:")
    );
    assert!(work.failure_accounted && work.receipts_reconciled_at.is_none());
}

#[tokio::test]
async fn genuine_late_command_restores_original_root_without_current_planner_authority() {
    let (db, schedule, _, parent, task_id, generation) = fixture().await;
    let original = original_run(&db, &parent.conversation_id).await;
    expire_as_unknown(&db, &schedule, &parent, &generation).await;
    let before = saved_root(&db, &parent.conversation_id).await;
    assert!(matches!(
        before.execution_state.states().as_slice(),
        [ExecutionState::OutcomeUnknown { .. }]
    ));
    let exec = crate::agent_exec_store::SignalAgentExecStore::new(db.clone());
    assert!(
        exec.finalize("another-host", &generation, &known_result())
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        exec.finalize("original-host", "stale-generation", &known_result())
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        !schedule
            .reconcile_late_task_receipts(&parent.conversation_id)
            .await
            .unwrap()
    );
    let terminal = exec
        .finalize("original-host", &generation, &known_result())
        .await
        .unwrap()
        .unwrap();
    let (output, receipt) = exec.command_result(&terminal).await.unwrap().unwrap();
    assert!(
        schedule
            .reconcile_late_task_receipts(&parent.conversation_id)
            .await
            .unwrap()
    );
    assert!(
        !schedule
            .reconcile_late_task_receipts(&parent.conversation_id)
            .await
            .unwrap()
    );
    let after = saved_root(&db, &parent.conversation_id).await;
    assert!(after.execution_state.states().is_empty() && after.unclosed_tool_call_ids().is_empty());
    assert_eq!(after.turn_state, before.turn_state);
    assert_eq!(after.terminal_error, before.terminal_error);
    assert_eq!(after.lease_token, before.lease_token);
    assert_eq!(after.control_revision, before.control_revision);
    assert_eq!(after.input_revision, before.input_revision);
    assert_eq!(after.current_turn_steps, before.current_turn_steps);
    assert_eq!(after.current_turn_tokens, before.current_turn_tokens);
    assert!(after.pending_auto_triggers.is_empty());
    let restored = after
        .conversation
        .iter()
        .find(|message| message.message_id == terminal.event_id)
        .unwrap();
    assert_eq!(restored.text, output.content);
    assert_eq!(restored.data_envelope.as_ref(), Some(&receipt.envelope));
    let work = original_run(&db, &parent.conversation_id).await;
    assert_eq!(work.status, "outcome_unknown");
    assert_eq!(work.started_at, original.started_at);
    assert_eq!(work.attempt, original.attempt);
    assert!(work.receipts_reconciled_at.is_some());
    let child = load(&db, &task_id).await;
    assert_eq!(child.state, SubAgentState::Cancelled);
    let group = group_row::Entity::find()
        .filter(group_row::Column::GroupId.eq(&child.binding.group_id))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(group.source_admission, "closed");
    assert!(!group.parent_active);
    assert_eq!(
        agent_exec_task::Entity::find_by_id(terminal.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap()
            .status,
        crate::agent_exec_store::STATUS_DONE
    );
}

#[tokio::test]
async fn closed_main_with_open_original_command_recovers_child_wait_after_lease_loss() {
    use desk_agent_protocol::ai_assistant::subagent::{AiAssistantStopControl, SubAgentStopChoice};
    let (db, schedule, store, parent, task_id, generation) = fixture().await;
    store
        .stop_for_owner(
            &parent.conversation_id,
            &parent.actor_id,
            &parent.device_id,
            &AiAssistantStopControl {
                client_request_id: "stop-original-planner".into(),
                expected_input_revision: parent.input_revision,
                expected_control_revision: parent.control_revision,
                subagent_choice: Some(SubAgentStopChoice::MainOnly),
            },
        )
        .await
        .unwrap();
    let native = crate::agent_exec_store::SignalAgentExecStore::new(db.clone())
        .finalize("original-host", &generation, &known_result())
        .await
        .unwrap()
        .unwrap();
    let original = original_run(&db, &parent.conversation_id).await;
    occurrence::Entity::update_many()
        .set(occurrence::ActiveModel {
            lease_deadline: Set(Some(chrono::Utc::now().timestamp_millis() - 1)),
            ..Default::default()
        })
        .filter(occurrence::Column::Id.eq(original.id))
        .exec(&db)
        .await
        .unwrap();
    assert!(
        schedule
            .recover_action_free_fresh_task(&parent.conversation_id)
            .await
            .unwrap()
    );
    let saved = saved_root(&db, &parent.conversation_id).await;
    assert!(saved.main_stopped && saved.pending_auto_triggers.is_empty());
    assert_eq!(saved.turn_state, TurnState::Cancelled);
    assert!(saved.execution_state.states().is_empty() && saved.unclosed_tool_call_ids().is_empty());
    assert!(
        saved
            .conversation
            .iter()
            .any(|message| message.message_id == native.event_id)
    );
    let work = original_run(&db, &parent.conversation_id).await;
    assert_eq!(work.status, "awaiting_children");
    assert!(
        work.result_ref
            .as_deref()
            .unwrap()
            .starts_with("stopped-children:")
    );
    assert_eq!(work.started_at, original.started_at);
    assert_eq!(work.attempt, original.attempt);
    assert!(!work.failure_accounted && work.finished_at.is_none());
    assert_eq!(load(&db, &task_id).await.state, SubAgentState::Queued);
}

#[tokio::test]
async fn missing_native_or_dispatch_proof_keeps_original_unknown_fact_unresolved() {
    for corruption in [
        "missing-native",
        "missing-origin",
        "wrong-generation",
        "wrong-publication",
        "future-control",
    ] {
        let (db, schedule, _, parent, _, generation) = fixture().await;
        expire_as_unknown(&db, &schedule, &parent, &generation).await;
        let native = crate::agent_exec_store::SignalAgentExecStore::new(db.clone())
            .finalize("original-host", &generation, &known_result())
            .await
            .unwrap()
            .unwrap();
        let before = saved_root(&db, &parent.conversation_id).await;
        let original_attempt = agent_action_item::Entity::find()
            .one(&db)
            .await
            .unwrap()
            .unwrap()
            .attempt;
        match corruption {
            "missing-native" => {
                agent_exec_task::Entity::delete_by_id(native.id)
                    .exec(&db)
                    .await
                    .unwrap();
            }
            "missing-origin" | "wrong-generation" | "future-control" => {
                let row = agent_capability_dispatch_outbox::Entity::find()
                    .filter(agent_capability_dispatch_outbox::Column::DispatchId.eq(&generation))
                    .one(&db)
                    .await
                    .unwrap()
                    .unwrap();
                let mut payload: crate::capability_grant_store::CapabilityDispatchPayload =
                    serde_json::from_str(&row.payload_json).unwrap();
                if corruption == "missing-origin" {
                    payload.command_origin = None;
                } else if corruption == "future-control" {
                    payload
                        .command_origin
                        .as_mut()
                        .unwrap()
                        .turn_fence
                        .control_revision = before.control_revision + 1;
                    payload.command_receipt = None;
                } else {
                    payload.dispatch_id = "another-generation".into();
                }
                agent_capability_dispatch_outbox::Entity::update_many()
                    .set(agent_capability_dispatch_outbox::ActiveModel {
                        payload_json: Set(serde_json::to_string(&payload).unwrap()),
                        ..Default::default()
                    })
                    .filter(agent_capability_dispatch_outbox::Column::Id.eq(row.id))
                    .exec(&db)
                    .await
                    .unwrap();
            }
            "wrong-publication" => {
                let row = agent_capability_grant::Entity::find()
                    .one(&db)
                    .await
                    .unwrap()
                    .unwrap();
                let mut grant: desk_agent_protocol::capability_grant::CapabilityGrant =
                    serde_json::from_str(&row.issued_payload_json).unwrap();
                let desk_agent_protocol::capability_grant::CapabilityGrantIssuer::TaskAuthorization(
                    source,
                ) = &mut grant.issued_by
                else {
                    panic!("published grant required");
                };
                source.scheduled_run_id = "another-occurrence".into();
                agent_capability_grant::Entity::update_many()
                    .set(agent_capability_grant::ActiveModel {
                        issued_payload_json: Set(serde_json::to_string(&grant).unwrap()),
                        ..Default::default()
                    })
                    .filter(agent_capability_grant::Column::Id.eq(row.id))
                    .exec(&db)
                    .await
                    .unwrap();
            }
            _ => unreachable!(),
        }
        assert!(
            !matches!(
                schedule
                    .reconcile_late_task_receipts(&parent.conversation_id)
                    .await,
                Ok(true)
            ),
            "{corruption}"
        );
        assert_eq!(
            saved_root(&db, &parent.conversation_id).await,
            before,
            "{corruption}"
        );
        assert!(
            original_run(&db, &parent.conversation_id)
                .await
                .receipts_reconciled_at
                .is_none()
        );
        assert_eq!(
            agent_action_item::Entity::find()
                .one(&db)
                .await
                .unwrap()
                .unwrap()
                .attempt,
            original_attempt
        );
    }
}
