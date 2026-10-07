//! Publication fixtures exercise real source capture, child claims and accounting.
use super::*;
use crate::entity::{
    agent_delegation_reservation as cost, agent_schedule_run as occurrence,
    agent_task_authorization as authorization, agent_task_budget_reservation as quota,
};
use crate::schedule_store::{
    ScheduleStore, TestPublicationVerifier, publication_device_test_fixture,
};
use desk_diagnose_core::{
    goal::GoalUsage,
    session::TriggerOrigin,
    subagent::{
        creation::CreationEnvelope,
        reservation::{CallAdmission, DelegationCallKind, DelegationCallReservation},
    },
};

async fn published_parent() -> (
    DatabaseConnection,
    ScheduleStore,
    SubAgentStore,
    PersistedAgentSession,
    CreationEnvelope,
) {
    published_parent_with_command(false).await
}

pub(super) async fn published_parent_with_command(
    command: bool,
) -> (
    DatabaseConnection,
    ScheduleStore,
    SubAgentStore,
    PersistedAgentSession,
    CreationEnvelope,
) {
    published_parent_on(database().await, command).await
}

async fn published_parent_on(
    db: DatabaseConnection,
    command: bool,
) -> (
    DatabaseConnection,
    ScheduleStore,
    SubAgentStore,
    PersistedAgentSession,
    CreationEnvelope,
) {
    let (schedule, mut task, contract, mut publication) =
        publication_device_test_fixture(db.clone(), Some("1")).await;
    if command {
        use desk_agent_protocol::schedule::contract::{
            TaskContract, TaskFixedStep, TaskInputConstraint, TaskPermissionRule,
            TaskPermissionScope, TaskStepBinding,
        };
        let registry = desk_diagnose_core::ai_assistant::ai_assistant_provider_registry();
        let capability = registry
            .capability_for_tool(desk_diagnose_core::command_confirmation::COMMAND_TOOL)
            .unwrap();
        let provider = registry
            .provider_for_capability(&capability.wire.capability_id)
            .unwrap();
        let mut definition: TaskContract = serde_json::from_str(&contract.canonical_json).unwrap();
        let scope = TaskPermissionScope {
            resources: vec!["device:1".into()],
            operations: vec!["execute".into()],
            export_destinations: vec![],
            limits: desk_agent_protocol::capability_grant::CapabilityGrantLimits {
                max_bytes_per_call: 4096,
                max_items_per_call: 1,
                max_calls: 1,
            },
        };
        definition.permissions.push(TaskPermissionRule {
            rule_id: "original-command".into(),
            provider_id: provider.wire.provider_id.clone(),
            capability_id: capability.wire.capability_id.clone(),
            tool_name: capability.wire.tool_name.clone(),
            tool_schema_version: capability.wire.input_schema_version,
            effect: capability.wire.effect,
            risk_tier: desk_agent_protocol::capability_grant::CapabilityRiskTier::R3,
            input: TaskInputConstraint::Exact {
                canonical_json: command_fixture_input(),
            },
            automatic: scope.clone(),
            approval_ceiling: scope,
        });
        definition.steps.push(TaskFixedStep {
            step_id: "original-command".into(),
            rule_id: "original-command".into(),
            depends_on: vec![],
            binding: TaskStepBinding::Exact,
        });
        let saved = schedule
            .save_contract(1, task.revision, &definition)
            .await
            .unwrap();
        task = schedule.read(1, &task.schedule_id).await.unwrap();
        publication.expected_revision = task.revision;
        publication.contract_revision = saved.contract_revision;
        publication.contract_sha256 = saved.digest_sha256;
    }
    schedule
        .publish_task(1, &publication, &TestPublicationVerifier(true))
        .await
        .unwrap();
    let queued = schedule
        .enqueue_manual(1, &task.schedule_id, "delegation-source")
        .await
        .unwrap();
    let txn = db.begin().await.unwrap();
    let work = ScheduleStore::claim_queued_on(&txn, &queued.run_id, "source-node", 90)
        .await
        .unwrap();
    let authority = ScheduleStore::lock_run_authority(
        &txn,
        1,
        "1",
        &work.run_id,
        "source-node",
        work.lease_epoch,
    )
    .await
    .unwrap();
    let initial = ScheduleStore::insert_fresh_session_on(&txn, &authority, 1, scope())
        .await
        .unwrap();
    txn.commit().await.unwrap();
    let store = SubAgentStore::new(db.clone());
    let (parent, source) = store
        .initialize_scheduled_source(
            &initial,
            "source-node",
            work.lease_epoch,
            &super::creation::destination(),
        )
        .await
        .unwrap();
    assert_eq!(initial.version, 1);
    assert_eq!(parent.version, 2);
    assert_eq!(parent.lease_token, initial.lease_token);
    assert_eq!(
        parent.conversation.as_slice(),
        std::slice::from_ref(&source.owner_requirement)
    );
    assert_eq!(parent.trigger_origin, TriggerOrigin::ScheduledTask);
    assert!(
        store
            .initialize_scheduled_source(
                &initial,
                "source-node",
                work.lease_epoch,
                &super::creation::destination()
            )
            .await
            .is_err()
    );
    (db, schedule, store, parent, source)
}

pub(super) fn command_fixture_input() -> String {
    desk_diagnose_core::permission_tools::canonical_tool_permission_input_json(
        desk_diagnose_core::command_confirmation::COMMAND_TOOL,
        serde_json::json!({"schema_version":1,"shell":"bash","command":"pwd","timeout_ms":60000}),
    )
    .unwrap()
}

pub(crate) mod command;

pub(crate) async fn awaiting_children_fixture() -> (
    DatabaseConnection,
    ScheduleStore,
    SubAgentStore,
    PersistedAgentSession,
    String,
) {
    use desk_diagnose_core::subagent::{
        tools::{self, Operation},
        wait::WaitMode,
    };
    let (db, schedule, store, mut parent, _) = published_parent().await;
    let request = super::creation::spawn_request();
    let call = super::main_tools::committed_call(
        &db,
        &mut parent,
        "scheduled-spawn",
        tools::SPAWN,
        serde_json::to_value(&request).unwrap(),
    )
    .await;
    store
        .execute_main_tool(
            &mut parent,
            &call,
            Operation::Spawn(request),
            "scheduled-spawn-result",
        )
        .await
        .unwrap();
    let row = run_row::Entity::find().one(&db).await.unwrap().unwrap();
    let task_id = row.task_id;
    let call = super::main_tools::committed_call(
        &db,
        &mut parent,
        "scheduled-wait",
        tools::WAIT,
        serde_json::json!({"task_ids": [&task_id], "mode": "all_terminal"}),
    )
    .await;
    store
        .execute_main_tool(
            &mut parent,
            &call,
            Operation::Wait {
                task_ids: vec![task_id.clone()],
                mode: WaitMode::AllTerminal,
            },
            "scheduled-wait-result",
        )
        .await
        .unwrap();
    let wait_id = parent.subagent_wait.as_ref().unwrap().wait_id.clone();
    parent.finish_turn(
        desk_diagnose_core::session::TurnState::Idle,
        chrono::Utc::now().to_rfc3339(),
    );
    parent.handled_input_seq = parent.latest_input_seq;
    save_main_delegation_session(&db, &mut parent)
        .await
        .unwrap();
    let waiting = schedule
        .await_fresh_task_children(
            crate::schedule_store::FreshTaskLease {
                owner: 1,
                run_id: &parent.conversation_id,
                node_id: "source-node",
                run_epoch: 1,
                session_token: parent.lease_token,
            },
            &wait_id,
        )
        .await
        .unwrap();
    assert_eq!(waiting.status, "awaiting_children");
    assert_eq!(
        waiting.result_ref.as_deref(),
        Some(format!("children:{wait_id}").as_str())
    );
    assert!(waiting.lease_owner.is_none());
    assert!(waiting.lease_deadline.is_none());
    assert!(waiting.finished_at.is_none());
    assert!(!waiting.failure_accounted);
    (db, schedule, store, parent, task_id)
}

pub(crate) async fn answered_children_fixture() -> (
    DatabaseConnection,
    ScheduleStore,
    SubAgentStore,
    PersistedAgentSession,
    String,
) {
    answered_children_fixture_at(None).await
}

pub(crate) async fn answered_children_fixture_at(
    url: Option<&str>,
) -> (
    DatabaseConnection,
    ScheduleStore,
    SubAgentStore,
    PersistedAgentSession,
    String,
) {
    use desk_diagnose_core::{
        chat::{ChatMessage, ChatRole, ModelTurn, StopReason},
        model_egress::ModelEgressPolicy,
        subagent::tools::{self, Operation},
    };
    let db = match url {
        Some(url) => database_at(url).await,
        None => database().await,
    };
    let (db, schedule, store, mut parent, _) = published_parent_on(db, false).await;
    let mut request = super::creation::spawn_request();
    request.required_for_completion = false;
    let call = super::main_tools::committed_call(
        &db,
        &mut parent,
        "scheduled-optional-spawn",
        tools::SPAWN,
        serde_json::to_value(&request).unwrap(),
    )
    .await;
    store
        .execute_main_tool(
            &mut parent,
            &call,
            Operation::Spawn(request),
            "scheduled-optional-result",
        )
        .await
        .unwrap();
    let task_id = run_row::Entity::find()
        .one(&db)
        .await
        .unwrap()
        .unwrap()
        .task_id;
    let policy = ModelEgressPolicy {
        destination: super::creation::destination(),
        selected_source_tools: Default::default(),
        export_authorization_id: "scheduled-initial-answer".into(),
        now_unix_ms: chrono::Utc::now().timestamp_millis() as u64,
        byte_cap: 1024 * 1024,
        permission_resume: false,
    };
    let text = "The independent investigation is still running.";
    let turn = ModelTurn {
        text: text.into(),
        stop_reason: StopReason::EndTurn,
        ..Default::default()
    };
    let mut answer = ChatMessage::text("scheduled-initial-answer", ChatRole::Assistant, text)
        .with_turn_id(parent.current_turn_id.clone().unwrap());
    answer.data_envelope = Some(
        policy
            .derive_model_output_envelope(
                &turn,
                &[parent.conversation[0].data_envelope.clone().unwrap()],
            )
            .unwrap(),
    );
    parent.conversation.push(answer);
    parent.current_turn_steps = 2;
    parent.lifetime_steps = 2;
    parent.finish_turn(
        desk_diagnose_core::session::TurnState::Idle,
        chrono::Utc::now().to_rfc3339(),
    );
    parent.handled_input_seq = parent.latest_input_seq;
    save_main_delegation_session(&db, &mut parent)
        .await
        .unwrap();
    let waiting = schedule
        .finish_answered_fresh_task(
            super::super::super::schedule_store::FreshTaskLease {
                owner: 1,
                run_id: &parent.conversation_id,
                node_id: "source-node",
                run_epoch: 1,
                session_token: parent.lease_token,
            },
            text,
        )
        .await
        .unwrap();
    assert_eq!(waiting.status, "awaiting_children");
    assert_eq!(
        waiting.result_ref.as_deref(),
        Some("answer-children:scheduled-initial-answer")
    );
    assert!(waiting.lease_owner.is_none() && waiting.lease_deadline.is_none());
    assert!(!waiting.failure_accounted && waiting.finished_at.is_none());
    (db, schedule, store, parent, task_id)
}

pub(crate) async fn complete_scheduled_child(db: &DatabaseConnection, task_id: &str) {
    super::main_tools::complete_child(db, task_id).await;
}

#[tokio::test]
async fn durable_occurrence_cancel_closes_only_its_children_and_preserves_source_evidence() {
    let (db, _, store, parent, task_id) = awaiting_children_fixture().await;
    let group = parent.delegation_group_id.as_deref().unwrap();
    assert!(
        store
            .withdrawn_scheduled_source_candidates(0, 32)
            .await
            .unwrap()
            .is_empty()
    );
    occurrence::Entity::update_many()
        .set(occurrence::ActiveModel {
            cancel_requested_at: Set(Some(chrono::Utc::now().timestamp_millis())),
            ..Default::default()
        })
        .filter(occurrence::Column::RunId.eq(&parent.conversation_id))
        .exec(&db)
        .await
        .unwrap();
    let candidates = store
        .withdrawn_scheduled_source_candidates(0, 1)
        .await
        .unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].group_id, group);
    assert_eq!(
        store
            .close_scheduled_source(&parent.conversation_id, &parent.actor_id, &parent.device_id)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        store
            .close_scheduled_source(&parent.conversation_id, &parent.actor_id, &parent.device_id)
            .await
            .unwrap(),
        0
    );
    let row = group_row::Entity::find()
        .filter(group_row::Column::GroupId.eq(group))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        decode_group(&row).unwrap().source_admission,
        SourceAdmission::Closed
    );
    assert!(!row.creation_envelope_json.is_empty());
    let child = load(&db, &task_id).await;
    assert_eq!(child.state, SubAgentState::Cancelled);
    assert_eq!(
        child.binding.source_epoch,
        decode_group(&row).unwrap().source_epoch
    );
    assert!(
        store
            .withdrawn_scheduled_source_candidates(0, 32)
            .await
            .unwrap()
            .is_empty()
    );
    let root = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq(parent.conversation_id))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert!(
        !PersistedAgentSession::decode_json(&root.state_json)
            .unwrap()
            .conversation
            .is_empty()
    );
}

async fn reserve(
    store: &SubAgentStore,
    parent: &PersistedAgentSession,
    logical_id: &str,
    kind: DelegationCallKind,
    tokens: u64,
) -> DelegationCallReservation {
    match store
        .reserve_runtime_call(
            parent,
            logical_id,
            kind,
            &"a".repeat(64),
            GoalUsage {
                input_tokens: tokens,
                model_calls: 1,
                ..Default::default()
            },
            chrono::Utc::now().timestamp_millis(),
        )
        .await
        .unwrap()
    {
        CallAdmission::Reserved(reservation) => reservation,
        other => panic!("one physical allocation, got {other:?}"),
    }
}

#[tokio::test]
async fn source_initialization_is_fenced_and_keeps_one_original_group() {
    let (db, _, store, parent, source) = published_parent().await;
    let (same, creation) = store
        .initialize_scheduled_source(&parent, "source-node", 1, &super::creation::destination())
        .await
        .unwrap();
    assert_eq!(same, parent);
    assert_eq!(creation, source);
    assert_eq!(group_row::Entity::find().count(&db).await.unwrap(), 1);
    let mut changed = super::creation::destination();
    if let desk_agent_protocol::data_lineage::DestinationIdentity::Model { model_id, .. } =
        &mut changed
    {
        *model_id = "other-model".into();
    }
    assert!(
        store
            .initialize_scheduled_source(&parent, "source-node", 1, &changed)
            .await
            .is_err()
    );
    assert!(
        store
            .initialize_scheduled_source(&parent, "other-node", 1, &source.model_destination)
            .await
            .is_err()
    );
    assert!(
        store
            .initialize_scheduled_source(&parent, "source-node", 2, &source.model_destination)
            .await
            .is_err()
    );
    authorization::Entity::update_many()
        .set(authorization::ActiveModel {
            revoked_at: Set(Some(chrono::Utc::now().timestamp_millis())),
            ..Default::default()
        })
        .exec(&db)
        .await
        .unwrap();
    assert!(
        store
            .initialize_scheduled_source(&parent, "source-node", 1, &source.model_destination)
            .await
            .is_err()
    );
    assert_eq!(group_row::Entity::find().count(&db).await.unwrap(), 1);
}

#[tokio::test]
async fn parent_compression_and_safety_share_the_occurrence_without_duplicate_run_charge() {
    let (db, _, store, parent, _) = published_parent().await;
    let mut reservations = Vec::new();
    for (id, kind) in [
        ("planning", DelegationCallKind::Model),
        ("compression", DelegationCallKind::ContextSummary),
        ("report-repair", DelegationCallKind::Model),
        ("safety", DelegationCallKind::SafetyReview),
    ] {
        reservations.push(reserve(&store, &parent, id, kind, 200).await);
    }
    let allocations = quota::Entity::find()
        .filter(quota::Column::Kind.eq("model_tokens"))
        .all(&db)
        .await
        .unwrap();
    assert_eq!(allocations.len(), 4);
    assert_eq!(
        allocations.iter().map(|row| row.charged_units).sum::<i64>(),
        800
    );
    assert_eq!(
        quota::Entity::find()
            .filter(quota::Column::Kind.eq("run"))
            .count(&db)
            .await
            .unwrap(),
        1
    );
    for reservation in &reservations {
        let row = cost::Entity::find()
            .filter(cost::Column::ReservationId.eq(&reservation.reservation_id))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert!(
            allocations
                .iter()
                .any(|allocation| Some(&allocation.reservation_id)
                    == row.source_schedule_budget_id.as_ref())
        );
    }
    let held = reserve(
        &store,
        &parent,
        "remaining-budget",
        DelegationCallKind::Model,
        9_200,
    )
    .await;
    assert_eq!(
        store
            .reserve_runtime_call(
                &parent,
                "over-budget",
                DelegationCallKind::Model,
                &"b".repeat(64),
                GoalUsage {
                    input_tokens: 1,
                    model_calls: 1,
                    ..Default::default()
                },
                chrono::Utc::now().timestamp_millis()
            )
            .await
            .unwrap(),
        CallAdmission::Exhausted
    );
    assert_eq!(cost::Entity::find().count(&db).await.unwrap(), 5);
    store
        .settle_runtime_call(&held, None, chrono::Utc::now().timestamp_millis())
        .await
        .unwrap();
    let remaining = quota::Entity::find()
        .filter(quota::Column::Kind.eq("model_tokens"))
        .all(&db)
        .await
        .unwrap();
    assert_eq!(
        remaining.iter().map(|row| row.charged_units).sum::<i64>(),
        800
    );
}

#[tokio::test]
async fn unknown_scheduled_usage_settles_once_after_cancel_without_reopening_authority() {
    let (db, _, store, parent, _) = published_parent().await;
    let receipt = reserve(
        &store,
        &parent,
        "unknown-provider",
        DelegationCallKind::Model,
        200,
    )
    .await;
    let now = chrono::Utc::now().timestamp_millis();
    store
        .link_model_receipt(&receipt, "original-provider", now)
        .await
        .unwrap();
    store
        .settle_runtime_call(&receipt, None, now + 1)
        .await
        .unwrap();
    let pending = quota::Entity::find()
        .filter(quota::Column::Kind.eq("model_tokens"))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pending.state, "reserved");
    assert_eq!(pending.charged_units, 200);
    occurrence::Entity::update_many()
        .set(occurrence::ActiveModel {
            cancel_requested_at: Set(Some(now + 2)),
            ..Default::default()
        })
        .filter(occurrence::Column::RunId.eq(&parent.conversation_id))
        .exec(&db)
        .await
        .unwrap();
    assert!(
        store
            .reserve_runtime_call(
                &parent,
                "after-cancel",
                DelegationCallKind::Model,
                &"a".repeat(64),
                GoalUsage {
                    input_tokens: 1,
                    model_calls: 1,
                    ..Default::default()
                },
                now + 3
            )
            .await
            .is_err()
    );
    super::funding::insert_provider_usage(&db, "original-provider", now, now + 17).await;
    store
        .settle_runtime_call(&receipt, None, now + 17)
        .await
        .unwrap();
    store
        .settle_runtime_call(&receipt, None, now + 18)
        .await
        .unwrap();
    let settled = quota::Entity::find_by_id(pending.id)
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(settled.state, "settled");
    assert_eq!(settled.charged_units, 18);
    assert_eq!(settled.version, 2);
    let run = occurrence::Entity::find()
        .filter(occurrence::Column::RunId.eq(parent.conversation_id))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(run.cancel_requested_at, Some(now + 2));
    assert_eq!(run.status, "running");
}

#[tokio::test]
async fn child_claim_and_quota_survive_parent_holder_release_but_not_source_cancellation() {
    use desk_diagnose_core::chat::{ChatMessage, ToolCall, ToolCallRef};
    let (db, _, store, mut parent, source) = published_parent().await;
    let request = super::creation::spawn_request();
    let call = ToolCall {
        id: "spawn-original-child".into(),
        name: desk_diagnose_core::subagent::tools::SPAWN.into(),
        arguments_json: serde_json::to_string(&request).unwrap(),
    };
    let mut caller = ChatMessage::assistant_tool_calls(
        "published-model-call",
        "Investigate independently",
        vec![ToolCallRef {
            id: call.id.clone(),
            name: call.name.clone(),
            arguments_json: call.arguments_json.clone(),
        }],
    );
    caller.turn_id = parent.current_turn_id.clone();
    caller.data_envelope = Some(
        desk_diagnose_core::subagent::projection::envelope(
            &caller.message_id,
            &caller.text,
            "model-output",
            &[source.owner_requirement.data_envelope.clone().unwrap()],
        )
        .unwrap(),
    );
    parent.conversation.push(caller);
    parent.version += 1;
    session_row::Entity::update_many()
        .set(session_row::ActiveModel {
            state_json: Set(parent.encode_json_for_storage().unwrap()),
            version: Set(parent.version),
            ..Default::default()
        })
        .filter(session_row::Column::ConversationId.eq(&parent.conversation_id))
        .exec(&db)
        .await
        .unwrap();
    let task = store
        .spawn_for_turn(&parent, &call, &request)
        .await
        .unwrap();
    let original = load(&db, &task.task_id).await;
    parent.finish_turn(
        desk_diagnose_core::session::TurnState::Idle,
        chrono::Utc::now().to_rfc3339(),
    );
    parent.version += 1;
    session_row::Entity::update_many()
        .set(session_row::ActiveModel {
            state_json: Set(parent.encode_json_for_storage().unwrap()),
            version: Set(parent.version),
            lease_deadline: Set(None),
            ..Default::default()
        })
        .filter(session_row::Column::ConversationId.eq(&parent.conversation_id))
        .exec(&db)
        .await
        .unwrap();
    occurrence::Entity::update_many()
        .set(occurrence::ActiveModel {
            status: Set("awaiting_children".into()),
            lease_owner: Set(None),
            lease_deadline: Set(None),
            lease_epoch: Set(2),
            ..Default::default()
        })
        .filter(occurrence::Column::RunId.eq(&parent.conversation_id))
        .exec(&db)
        .await
        .unwrap();
    let params = super::creation::claim_params(&original);
    let claimed = match store
        .claim_child(
            &params,
            &task.task_id,
            original.fence(),
            &source.model_destination,
        )
        .await
        .unwrap()
    {
        SubAgentClaimOutcome::Claimed(claimed) => claimed,
        _ => panic!("independent child holder"),
    };
    assert!(
        claimed.session.delegated_owner_requirement.as_ref() == Some(&source.owner_requirement)
    );
    let receipt = reserve(
        &store,
        &claimed.session,
        "child-provider",
        DelegationCallKind::Model,
        200,
    )
    .await;
    let txn = db.begin().await.unwrap();
    lock_child_source_on(&txn, &claimed.session).await.unwrap();
    txn.rollback().await.unwrap();
    occurrence::Entity::update_many()
        .set(occurrence::ActiveModel {
            cancel_requested_at: Set(Some(chrono::Utc::now().timestamp_millis())),
            ..Default::default()
        })
        .filter(occurrence::Column::RunId.eq(&parent.conversation_id))
        .exec(&db)
        .await
        .unwrap();
    let txn = db.begin().await.unwrap();
    assert!(lock_child_source_on(&txn, &claimed.session).await.is_err());
    txn.rollback().await.unwrap();
    assert!(
        store
            .link_model_receipt(
                &receipt,
                "child-after-cancel",
                chrono::Utc::now().timestamp_millis()
            )
            .await
            .is_err()
    );
    assert_eq!(
        quota::Entity::find()
            .filter(quota::Column::Kind.eq("run"))
            .count(&db)
            .await
            .unwrap(),
        1
    );
}

pub(crate) async fn record_test_notice_answer(
    db: &DatabaseConnection,
    parent: &mut PersistedAgentSession,
    receipt: bool,
) {
    super::notification::answer(db, parent, receipt).await;
}

#[tokio::test]
async fn deleted_schedule_and_redacted_conversation_retain_only_the_original_budget_link_for_late_usage()
 {
    let (db, schedule, store, parent, _) = published_parent().await;
    let receipt = reserve(
        &store,
        &parent,
        "redacted-scheduled-provider",
        DelegationCallKind::Model,
        200,
    )
    .await;
    let now = chrono::Utc::now().timestamp_millis();
    store
        .link_model_receipt(&receipt, "original-provider", now)
        .await
        .unwrap();
    store
        .settle_runtime_call(&receipt, None, now + 1)
        .await
        .unwrap();
    let quota_before = quota::Entity::find()
        .filter(quota::Column::Kind.eq("model_tokens"))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let work = occurrence::Entity::find()
        .filter(occurrence::Column::RunId.eq(&parent.conversation_id))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let task = schedule.read(1, &work.schedule_id).await.unwrap();
    schedule
        .delete(1, &work.schedule_id, task.revision)
        .await
        .unwrap();
    let txn = db.begin().await.unwrap();
    close_root_on(&txn, &parent, now + 2).await.unwrap();
    session_row::Entity::delete_many()
        .filter(session_row::Column::ConversationId.eq(&parent.conversation_id))
        .exec(&txn)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    assert_eq!(redact_deleted_content(&db, now + 60_000).await.unwrap(), 1);
    assert_eq!(redact_deleted_content(&db, now + 60_000).await.unwrap(), 0);
    assert_eq!(purge_groups(&db, now + 60_000).await.unwrap(), 0);
    let group = group_row::Entity::find()
        .filter(group_row::Column::GroupId.eq(&receipt.group_id))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert!(group.creation_envelope_json.is_empty() && group.content_redacted_at_ms.is_some());
    assert_eq!(
        group.source_schedule_id.as_deref(),
        Some(work.schedule_id.as_str())
    );
    assert_eq!(
        group.source_occurrence_id.as_deref(),
        Some(work.run_id.as_str())
    );
    assert_eq!(group.source_admission, "closed");
    assert_eq!(
        quota::Entity::find_by_id(quota_before.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap(),
        quota_before
    );
    super::funding::insert_provider_usage(&db, "original-provider", now, now + 17).await;
    store
        .settle_runtime_call(&receipt, None, now + 17)
        .await
        .unwrap();
    store
        .settle_runtime_call(&receipt, None, now + 18)
        .await
        .unwrap();
    let settled = quota::Entity::find_by_id(quota_before.id)
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(settled.state, "settled");
    assert_eq!(settled.charged_units, 18);
    assert_eq!(settled.version, 2);
    assert_eq!(
        schedule.read(1, &work.schedule_id).await.unwrap().status,
        "deleted"
    );
    assert!(
        store
            .reserve_runtime_call(
                &parent,
                "cannot-revive-redacted-source",
                DelegationCallKind::Model,
                &"a".repeat(64),
                GoalUsage {
                    input_tokens: 1,
                    model_calls: 1,
                    ..Default::default()
                },
                now + 19
            )
            .await
            .is_err()
    );
}
