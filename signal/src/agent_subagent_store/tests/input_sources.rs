use super::*;
use crate::agent_goal_store as goals;
use desk_diagnose_core::{
    chat::{ChatMessage, ToolCall, ToolCallRef},
    goal::{GoalModelBinding, GoalOwnerAction, GoalRun, GoalUsage},
    session::TurnState,
    subagent::{
        DelegationSource,
        reservation::{CallAdmission, DelegationCallKind},
    },
};

pub(super) async fn add_input_tables(db: &DatabaseConnection) {
    let schema = Schema::new(db.get_database_backend());
    for mut statement in [
        schema.create_table_from_entity(crate::entity::agent_run_event::Entity),
        schema.create_table_from_entity(crate::entity::agent_goal_open_request::Entity),
    ] {
        db.execute(statement.if_not_exists()).await.unwrap();
    }
}

async fn root(db: &DatabaseConnection) -> PersistedAgentSession {
    let row = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq("root"))
        .one(db)
        .await
        .unwrap()
        .unwrap();
    PersistedAgentSession::decode_json(&row.state_json).unwrap()
}

// Represents a completed parent planning slice; no child state or source is rewritten.
pub(super) async fn release_parent(db: &DatabaseConnection) {
    let row = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq("root"))
        .one(db)
        .await
        .unwrap()
        .unwrap();
    let mut session = PersistedAgentSession::decode_json(&row.state_json).unwrap();
    session.finish_turn(TurnState::Idle, chrono::Utc::now().to_rfc3339());
    session.version += 1;
    session_row::Entity::update_many()
        .set(session_row::ActiveModel {
            state_json: Set(session.encode_json_for_storage().unwrap()),
            version: Set(session.version),
            lease_deadline: Set(None),
            ..Default::default()
        })
        .filter(session_row::Column::Id.eq(row.id))
        .filter(session_row::Column::Version.eq(row.version))
        .exec(db)
        .await
        .unwrap();
}

fn model() -> GoalModelBinding {
    GoalModelBinding::from_destination(&super::creation::destination()).unwrap()
}

pub(super) async fn append_input(db: &DatabaseConnection, start: bool, id: &str) {
    let now = chrono::Utc::now();
    let message = desk_diagnose_core::model_message_labels::model_bound_user_message(
        id.into(),
        if start {
            "Investigate a new goal"
        } else {
            "Continue with this additional evidence"
        }
        .into(),
        super::creation::destination(),
    )
    .unwrap();

    use crate::agent_run_event_store::{
        AppendUserFollowupParams, SignalAgentRunEventStore, StartUserGoal,
    };
    let params = AppendUserFollowupParams {
        event_id: format!("input-{id}"),
        run_id: "root".into(),
        actor_id: "1".into(),
        device_id: "1".into(),
        client_conversation_id: None,
        surface: AgentSessionSurface::AiAssistant,
        policy_revision: 2,
        current_scope: scope(),
        read_context: None,
        message,
        created_at: now.to_rfc3339(),
    };
    let inputs = SignalAgentRunEventStore::new(db.clone());
    if start {
        inputs
            .append_user_goal(
                params,
                StartUserGoal {
                    goal_id: "new-goal".into(),
                    previous_completed_goal_id: None,
                    model_binding: model(),
                },
            )
            .await
            .unwrap();
    } else {
        inputs.append_user_followup(params).await.unwrap();
    }
}

pub(super) async fn goal(db: &DatabaseConnection) -> GoalRun {
    let row = goal_row::Entity::find()
        .filter(goal_row::Column::GoalId.eq("new-goal"))
        .one(db)
        .await
        .unwrap()
        .unwrap();
    goals::decode(&row).unwrap()
}

pub(super) async fn spawn_goal_child(
    db: &DatabaseConnection,
    store: &SubAgentStore,
) -> AiAssistantSubAgentSummary {
    let mut parent = root(db).await;
    let now = chrono::Utc::now();
    parent
        .begin_turn(
            "goal-parent-turn",
            Some("goal-parent-request".into()),
            None,
            2,
            scope(),
            now.to_rfc3339(),
        )
        .unwrap();
    let call = ToolCall {
        id: "new-goal-spawn".into(),
        name: desk_diagnose_core::subagent::tools::SPAWN.into(),
        arguments_json: serde_json::to_string(&super::creation::spawn_request()).unwrap(),
    };
    let mut caller = ChatMessage::assistant_tool_calls(
        "new-goal-caller",
        "Delegate the new goal",
        vec![ToolCallRef {
            id: call.id.clone(),
            name: call.name.clone(),
            arguments_json: call.arguments_json.clone(),
        }],
    );
    caller.turn_id = parent.current_turn_id.clone();
    let sources = parent
        .conversation
        .iter()
        .filter_map(|message| message.data_envelope.clone())
        .collect::<Vec<_>>();
    caller.data_envelope = Some(
        desk_diagnose_core::subagent::projection::envelope(
            &caller.message_id,
            &caller.text,
            "model-output",
            &sources,
        )
        .unwrap(),
    );
    parent.conversation.push(caller);
    parent.version += 1;
    session_row::Entity::update_many()
        .set(session_row::ActiveModel {
            state_json: Set(parent.encode_json_for_storage().unwrap()),
            version: Set(parent.version),
            lease_token: Set(parent.lease_token as i64),
            lease_deadline: Set(Some(now + chrono::Duration::minutes(5))),
            ..Default::default()
        })
        .filter(session_row::Column::ConversationId.eq("root"))
        .exec(db)
        .await
        .unwrap();
    store
        .spawn_for_turn(&parent, &call, &super::creation::spawn_request())
        .await
        .unwrap()
}

#[tokio::test]
async fn new_goal_and_later_input_preserve_old_child_sources_and_pause_only_the_goal() {
    let db = database().await;
    add_input_tables(&db).await;
    let (parent, calls) = super::creation::runnable_parent(&db).await;
    let store = SubAgentStore::new(db.clone());
    let ordinary = store
        .spawn_for_turn(&parent, &calls[0], &super::creation::spawn_request())
        .await
        .unwrap();
    let ordinary_before = load(&db, &ordinary.task_id).await;
    let ordinary_group_row = group_row::Entity::find()
        .filter(group_row::Column::GroupId.eq(&ordinary.group_id))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let ordinary_group = decode_group(&ordinary_group_row).unwrap();
    release_parent(&db).await;

    append_input(&db, true, "new-goal-owner-input").await;
    assert_eq!(load(&db, &ordinary.task_id).await, ordinary_before);
    let opened = goal(&db).await;
    assert_eq!(opened.input_revision, 2);
    let delegated = spawn_goal_child(&db, &store).await;
    let goal_child_before = load(&db, &delegated.task_id).await;
    assert!(
        matches!(&goal_child_before.binding.source, DelegationSource::Goal { goal_id } if goal_id == &opened.goal_id)
    );
    assert_ne!(ordinary.group_id, delegated.group_id);
    release_parent(&db).await;

    append_input(&db, false, "later-main-owner-input").await;
    let revised = goal(&db).await;
    assert_eq!(revised.input_revision, 3);
    assert_eq!(revised.used, opened.used);
    assert_eq!(revised.deadline_unix_ms, opened.deadline_unix_ms);
    assert_eq!(load(&db, &delegated.task_id).await, goal_child_before);
    let SubAgentClaimOutcome::Claimed(goal_claim) = store
        .claim_child(
            &super::creation::claim_params(&goal_child_before),
            &delegated.task_id,
            goal_child_before.fence(),
            &super::creation::destination(),
        )
        .await
        .unwrap()
    else {
        panic!("new main input preserves the original goal child");
    };

    let now = chrono::Utc::now();
    let paused = goals::apply_owner_action(
        &db,
        "root",
        "1",
        "1",
        &revised.goal_id,
        revised.state_version,
        GoalOwnerAction::Pause,
        now,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(paused.state, GoalState::Paused(GoalPauseReason::Owner));
    let waiting = load(&db, &delegated.task_id).await;
    assert_eq!(waiting.state, SubAgentState::WaitingSource);
    assert!(waiting.source_paused);
    assert!(waiting.binding.source_epoch > goal_child_before.binding.source_epoch);
    assert_eq!(
        waiting.binding.deadline_ms,
        goal_child_before.binding.deadline_ms
    );
    assert_eq!(
        store.child_admission(&goal_claim.session).await.unwrap(),
        desk_diagnose_core::subagent::seam::ChildAdmission::SourcePaused
    );
    assert_eq!(load(&db, &ordinary.task_id).await, ordinary_before);
    let row = group_row::Entity::find()
        .filter(group_row::Column::GroupId.eq(&ordinary.group_id))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(decode_group(&row).unwrap(), ordinary_group);

    let SubAgentClaimOutcome::Claimed(ordinary_claim) = store
        .claim_child(
            &super::creation::claim_params(&ordinary_before),
            &ordinary.task_id,
            ordinary_before.fence(),
            &super::creation::destination(),
        )
        .await
        .unwrap()
    else {
        panic!("the old ordinary source is independent of the new goal");
    };
    let upper = GoalUsage {
        input_tokens: 10,
        model_calls: 1,
        ..Default::default()
    };
    let CallAdmission::Reserved(receipt) = store
        .reserve_runtime_call(
            &ordinary_claim.session,
            "original-ordinary-model-call",
            DelegationCallKind::Model,
            &"a".repeat(64),
            upper,
            chrono::Utc::now().timestamp_millis(),
        )
        .await
        .unwrap()
    else {
        panic!("the old child keeps its original finite budget");
    };
    assert!(receipt.source_goal_upper.is_none());
    assert!(goal(&db).await.delegation_reservations.is_empty());
    store
        .settle_runtime_call(&receipt, None, chrono::Utc::now().timestamp_millis())
        .await
        .unwrap();
    assert_eq!(
        goal(&db).await.state,
        GoalState::Paused(GoalPauseReason::Owner)
    );

    let resumed = goals::apply_owner_action(
        &db,
        "root",
        "1",
        "1",
        &paused.goal_id,
        paused.state_version,
        GoalOwnerAction::Resume,
        chrono::Utc::now(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(resumed.used, paused.used);
    assert_eq!(resumed.deadline_unix_ms, paused.deadline_unix_ms);
    let ready = load(&db, &delegated.task_id).await;
    assert_eq!(ready.state, SubAgentState::Queued);
    assert!(!ready.source_paused);
    assert!(ready.binding.source_epoch > waiting.binding.source_epoch);
    assert_eq!(
        ready.binding.deadline_ms,
        goal_child_before.binding.deadline_ms
    );
    let mut params = super::creation::claim_params(&ready);
    params.turn_id = "resumed-original-goal-child".into();
    params.request_id = Some("resumed-original-goal-request".into());
    let SubAgentClaimOutcome::Claimed(claim) = store
        .claim_child(
            &params,
            &delegated.task_id,
            ready.fence(),
            &super::creation::destination(),
        )
        .await
        .unwrap()
    else {
        panic!("resume reclaims the same finite goal child");
    };
    assert!(claim.session.lease_token > goal_claim.session.lease_token);
    assert_eq!(
        claim.session.agent_role.binding().unwrap().deadline_ms,
        goal_child_before.binding.deadline_ms
    );
    assert_eq!(
        load(&db, &ordinary.task_id).await.state,
        SubAgentState::Running
    );
}

#[tokio::test]
async fn promoting_the_same_input_to_a_goal_does_not_adopt_an_existing_ordinary_child() {
    use desk_diagnose_core::goal::{GoalLimits, GoalOpening};
    let db = database().await;
    add_input_tables(&db).await;
    let (mut parent, calls) = super::creation::runnable_parent(&db).await;
    let store = SubAgentStore::new(db.clone());
    let original = store
        .spawn_for_turn(&parent, &calls[0], &super::creation::spawn_request())
        .await
        .unwrap();
    let before = load(&db, &original.task_id).await;
    let original_group_row = group_row::Entity::find()
        .filter(group_row::Column::GroupId.eq(&original.group_id))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let original_group = decode_group(&original_group_row).unwrap();
    let now = chrono::Utc::now();
    let promoted = GoalRun::new(
        "new-goal".into(),
        "root".into(),
        "1".into(),
        "1".into(),
        "Investigate these independent symptoms".into(),
        "source-message".into(),
        GoalOpening::OwnerRequest,
        model(),
        1,
        now.timestamp_millis() as u64,
        GoalLimits::default(),
    )
    .unwrap();
    let txn = db.begin().await.unwrap();
    goals::insert_on(&txn, &promoted).await.unwrap();
    initialize_goal_group_on(&txn, &mut parent, &promoted, now.timestamp_millis())
        .await
        .unwrap();
    parent.version += 1;
    session_row::Entity::update_many()
        .set(session_row::ActiveModel {
            state_json: Set(parent.encode_json_for_storage().unwrap()),
            version: Set(parent.version),
            ..Default::default()
        })
        .filter(session_row::Column::ConversationId.eq("root"))
        .exec(&txn)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    assert_ne!(
        parent.delegation_group_id.as_deref(),
        Some(original.group_id.as_str())
    );
    assert_eq!(load(&db, &original.task_id).await, before);
    release_parent(&db).await;
    let paused = goals::apply_owner_action(
        &db,
        "root",
        "1",
        "1",
        &promoted.goal_id,
        promoted.state_version,
        GoalOwnerAction::Pause,
        chrono::Utc::now(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(paused.state, GoalState::Paused(GoalPauseReason::Owner));
    let row = group_row::Entity::find()
        .filter(group_row::Column::GroupId.eq(&original.group_id))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(decode_group(&row).unwrap(), original_group);
    assert_eq!(load(&db, &original.task_id).await, before);
    let SubAgentClaimOutcome::Claimed(claim) = store
        .claim_child(
            &super::creation::claim_params(&before),
            &original.task_id,
            before.fence(),
            &super::creation::destination(),
        )
        .await
        .unwrap()
    else {
        panic!("promotion never copies authority into the original ordinary task");
    };
    assert_eq!(
        claim.session.agent_role.binding().unwrap().source,
        before.binding.source
    );
    assert_eq!(
        claim.session.agent_role.binding().unwrap().deadline_ms,
        before.binding.deadline_ms
    );
}

#[tokio::test]
async fn new_input_clears_pending_or_ready_parent_wait_without_losing_child_facts() {
    use crate::entity::agent_subagent_inbox as inbox;
    for ready in [false, true] {
        let db = database().await;
        add_input_tables(&db).await;
        let (parent, task_id) = super::parent_wait::waiting_parent(&db).await;
        let group_id = parent.delegation_group_id.clone().unwrap();
        release_parent(&db).await;
        if ready {
            super::main_tools::complete_child(&db, &task_id).await;
            assert!(matches!(
                SubAgentStore::new(db.clone())
                    .resolve_parent_wait("root", "1", "1")
                    .await
                    .unwrap(),
                ParentWaitResolution::Ready(_)
            ));
        }
        let before = root(&db).await;
        assert_eq!(before.subagent_wait.is_some(), !ready);
        assert_eq!(before.ready_subagent_wait.is_some(), ready);
        let child_before = load(&db, &task_id).await;
        let group_before = group_row::Entity::find()
            .filter(group_row::Column::GroupId.eq(&group_id))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let group_before = decode_group(&group_before).unwrap();
        let inbox_before = inbox::Entity::find()
            .all(&db)
            .await
            .unwrap()
            .into_iter()
            .map(|r| (r.id, r))
            .collect::<std::collections::BTreeMap<_, _>>();
        append_input(&db, false, "new-input-during-child-wait").await;
        let revised = root(&db).await;
        assert_eq!(revised.input_revision, before.input_revision + 1);
        assert!(revised.subagent_wait.is_none());
        assert!(revised.ready_subagent_wait.is_none());
        assert!(revised.ready_subagent_notification.is_none());
        assert_eq!(load(&db, &task_id).await, child_before);
        let group_after = group_row::Entity::find()
            .filter(group_row::Column::GroupId.eq(&group_id))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let group_after = decode_group(&group_after).unwrap();
        assert_eq!(group_after.budget, group_before.budget);
        assert_eq!(group_after.limits, group_before.limits);
        assert_eq!(group_after.source, group_before.source);
        let inbox_after = inbox::Entity::find()
            .all(&db)
            .await
            .unwrap()
            .into_iter()
            .map(|r| (r.id, r))
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(inbox_after, inbox_before);
        assert_eq!(
            SubAgentStore::new(db.clone())
                .resolve_parent_wait("root", "1", "1")
                .await
                .unwrap(),
            ParentWaitResolution::NotWaiting
        );
    }
}
