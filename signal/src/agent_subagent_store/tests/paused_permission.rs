//! Owner approval is durable while its original goal source is paused.
use super::*;
use desk_agent_protocol::capability_provider::{CapabilityEffect, ProductSurface};
use desk_diagnose_core::{
    capability_availability::CapabilityAvailability,
    dynamic_run::*,
    goal::{GoalOwnerAction, GoalRun},
    permission_grant::PermissionGrantIssuanceContext,
    session::TurnState,
};

pub(super) fn request(session: &mut PersistedAgentSession) -> PermissionRequestedEvent {
    let request = PermissionRequest {
        schema_version: PERMISSION_REQUEST_SCHEMA_VERSION,
        request_id: "paused-child-permission".into(),
        input_revision: session.input_revision,
        state: PermissionRequestState::Pending,
        items: vec![GrantRequestItem {
            command_confirmation: None,
            launch_confirmation: None,
            item_id: "inspect".into(),
            provider_id: "desktop.session".into(),
            tool_name: "inspect_desktop_session".into(),
            expected_effect: CapabilityEffect::ReadDevice,
            resource_scope: vec!["target:current_device".into()],
            operation_scope: vec!["observe".into()],
            export_destinations: vec![],
            canonical_input_json: None,
            canonical_input_digest_sha256: None,
            suggested_ttl_seconds: 120,
            suggested_max_uses: 1,
            reason: "Inspect original delegated target".into(),
        }],
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    session.add_permission_request(request.clone()).unwrap();
    session.last_event_seq += 1;
    PermissionRequestedEvent {
        event: AgentRunEvent {
            schema_version: AGENT_RUN_EVENT_SCHEMA_VERSION,
            event_id: "paused-child-request-event".into(),
            run_id: session.conversation_id.clone(),
            event_seq: session.last_event_seq,
            input_revision: session.input_revision,
            kind: AgentRunEventKind::PermissionRequested,
            correlation_id: Some(request.request_id.clone()),
            source_envelope_ids: vec![],
            result_envelope_ids: vec![],
            created_at: request.created_at.clone(),
        },
        request,
    }
}

fn decisions() -> Vec<PermissionDecisionItem> {
    vec![PermissionDecisionItem {
        item_id: "inspect".into(),
        decision: PermissionItemDecision::Approve {
            resource_scope: vec!["target:current_device".into()],
            operation_scope: vec!["observe".into()],
            export_destinations: vec![],
            ttl_seconds: 120,
            max_uses: 1,
        },
    }]
}

fn inventory() -> Vec<CapabilityAvailability> {
    vec![CapabilityAvailability {
        provider_id: "desktop.session".into(),
        capability_id: "desktop.session.inspect".into(),
        tool_name: "inspect_desktop_session".into(),
        compiled: true,
        enabled: true,
        connected: true,
        ready: true,
        reason: None,
    }]
}

pub(super) async fn session(db: &DatabaseConnection, id: &str) -> PersistedAgentSession {
    let row = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq(id))
        .one(db)
        .await
        .unwrap()
        .unwrap();
    PersistedAgentSession::decode_json(&row.state_json).unwrap()
}

#[tokio::test]
async fn approval_during_goal_pause_is_retained_without_resume_or_budget_reset() {
    let db = database().await;
    super::input_sources::add_input_tables(&db).await;
    db.execute(
        Schema::new(db.get_database_backend())
            .create_table_from_entity(grant_row::Entity)
            .if_not_exists(),
    )
    .await
    .unwrap();
    let (parent, calls) = super::creation::runnable_parent(&db).await;
    let store = SubAgentStore::new(db.clone());
    let other = store
        .spawn_for_turn(&parent, &calls[0], &super::creation::spawn_request())
        .await
        .unwrap();
    let other_before = load(&db, &other.task_id).await;
    super::input_sources::release_parent(&db).await;
    super::input_sources::append_input(&db, true, "permission-goal-input").await;
    let task = super::input_sources::spawn_goal_child(&db, &store).await;
    let opened = super::input_sources::goal(&db).await;
    assert!(
        control(&db, &opened, GoalOwnerAction::Pause)
            .await
            .is_none()
    );
    assert_eq!(super::input_sources::goal(&db).await, opened);
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
        panic!("child claim")
    };
    let mut child = claimed.session;
    // The low-level store fixture uses an arbitrary policy revision; the real
    // permission boundary requires the current compiled Assistant policy.
    child.policy_revision =
        desk_diagnose_core::assistant_policy::PERSONAL_ASSISTANT_POLICY_REVISION;
    save_child_session(&db, &mut child).await.unwrap();
    let event = request(&mut child);
    publish(&db, &mut child, &event).await;
    child.finish_turn(TurnState::Idle, chrono::Utc::now().to_rfc3339());
    save_child_session(&db, &mut child).await.unwrap();
    let original_group = group_row::Entity::find()
        .filter(group_row::Column::GroupId.eq(&task.group_id))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let original_group = decode_group(&original_group).unwrap();
    let paused = control(&db, &opened, GoalOwnerAction::Pause).await.unwrap();
    assert_eq!(paused.state, GoalState::Paused(GoalPauseReason::Owner));
    assert_eq!(load(&db, &other.task_id).await, other_before);
    approve(&db, &child.conversation_id).await;
    let recorded = session(&db, &child.conversation_id).await;
    assert_eq!(
        recorded.permission_requests[0].state,
        PermissionRequestState::Approved
    );
    assert_eq!(recorded.input_revision, child.input_revision);
    assert!(!recorded.turn_state.is_active());
    for _ in 0..2 {
        assert!(!pending(&db, &child.conversation_id).await);
        assert!(
            store
                .child_runtime_candidate(&task.task_id)
                .await
                .unwrap()
                .is_none()
        );
    }
    assert_eq!(
        load(&db, &task.task_id).await.state,
        SubAgentState::WaitingSource
    );
    assert_claim(&db, &child.conversation_id, false).await;
    let grants = grant_row::Entity::find().all(&db).await.unwrap();
    assert_eq!(grants.len(), 1);
    let resumed = control(&db, &paused, GoalOwnerAction::Resume)
        .await
        .unwrap();
    assert_eq!(resumed.used, opened.used);
    assert_eq!(resumed.limits, opened.limits);
    assert_eq!(resumed.deadline_unix_ms, opened.deadline_unix_ms);
    assert!(pending(&db, &child.conversation_id).await);
    assert_eq!(grant_row::Entity::find().all(&db).await.unwrap(), grants);
    let restored = session(&db, &child.conversation_id).await;
    assert_eq!(restored.permission_requests, recorded.permission_requests);
    assert_eq!(restored.permission_decisions, recorded.permission_decisions);
    let after = load(&db, &task.task_id).await;
    assert_eq!(after.binding.objective, run.binding.objective);
    assert_eq!(after.binding.deadline_ms, run.binding.deadline_ms);
    let current_group = group_row::Entity::find()
        .filter(group_row::Column::GroupId.eq(&task.group_id))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let current_group = decode_group(&current_group).unwrap();
    assert_eq!(current_group.budget, original_group.budget);
    assert_eq!(current_group.limits, original_group.limits);
    assert_eq!(load(&db, &other.task_id).await, other_before);
    assert_claim(&db, &child.conversation_id, true).await;
}

use crate::{
    agent_session_store::SignalAgentSessionStore, entity::agent_capability_grant as grant_row,
};
use desk_diagnose_core::seam::SessionSeam;

pub(super) async fn control(
    db: &DatabaseConnection,
    goal: &GoalRun,
    action: GoalOwnerAction,
) -> Option<GoalRun> {
    crate::agent_goal_store::apply_owner_action(
        db,
        "root",
        "1",
        "1",
        &goal.goal_id,
        goal.state_version,
        action,
        chrono::Utc::now(),
    )
    .await
    .unwrap()
}
pub(super) async fn publish(
    db: &DatabaseConnection,
    child: &mut PersistedAgentSession,
    event: &PermissionRequestedEvent,
) {
    SignalAgentSessionStore::new(db.clone())
        .save_permission_request(child, event)
        .await
        .unwrap();
}
pub(super) async fn approve(db: &DatabaseConnection, id: &str) {
    assert!(decide_at(db, id).await);
}
async fn decide_at(db: &DatabaseConnection, id: &str) -> bool {
    let registry = desk_diagnose_core::ai_assistant::ai_assistant_provider_registry();
    let inventory = inventory();
    let now = chrono::Utc::now();
    let ctx = PermissionGrantIssuanceContext {
        surface: ProductSurface::OssPersonalOwner,
        registry: &registry,
        inventory: &inventory,
        readiness_revision: 1,
        now_unix_ms: now.timestamp_millis() as u64,
        implicit_fresh_object_refs: &[],
    };
    SignalAgentSessionStore::new(db.clone())
        .decide_permission_request(
            id,
            "1",
            "1",
            "paused-child-permission",
            decisions(),
            ctx,
            &now.to_rfc3339(),
        )
        .await
        .is_ok()
}
async fn pending(db: &DatabaseConnection, id: &str) -> bool {
    use crate::entity::agent_permission_resume as resume;
    let candidate = resume::Entity::find()
        .filter(resume::Column::RunId.eq(id))
        .filter(resume::Column::RequestId.eq("paused-child-permission"))
        .one(db)
        .await
        .unwrap()
        .unwrap();
    SignalAgentSessionStore::new(db.clone())
        .pending_permission_resume(&candidate, chrono::Utc::now())
        .await
        .unwrap()
        .is_some()
}

pub(super) async fn assert_claim(db: &DatabaseConnection, id: &str, should_run: bool) {
    use desk_diagnose_core::seam::{ClaimError, ClaimTurnParams};
    let current = session(db, id).await;
    let grants = crate::capability_grant_store::SignalCapabilityGrantStore::new(db.clone())
        .list_for_subject(id, "1", "1")
        .await
        .unwrap();
    let params = ClaimTurnParams {
        conversation_id: id.into(),
        actor_id: "1".into(),
        device_id: "1".into(),
        policy_revision: current.policy_revision,
        current_pdp_scope: current.scope_snapshot.clone(),
        turn_id: crate::agent_session_store::permission_resume::turn_id(
            id,
            "paused-child-permission",
        ),
        request_id: Some("resumed-child-request".into()),
        connection_id: None,
        trigger_origin: desk_diagnose_core::session::TriggerOrigin::PermissionDecision,
        now: chrono::Utc::now().to_rfc3339(),
    };
    let result = SignalAgentSessionStore::new(db.clone())
        .with_client_metadata(current.client_conversation_id.clone(), current.surface)
        .with_expected_input_revision(current.input_revision)
        .with_permission_resume("paused-child-permission".into(), current.version, grants)
        .claim_turn(params)
        .await;
    if should_run {
        let resumed = result.unwrap();
        assert_eq!(resumed.agent_role, current.agent_role);
        assert!(resumed.turn_state.is_active());
    } else {
        assert!(matches!(result, Err(ClaimError::Busy)));
        assert_eq!(
            session(db, id).await.encode_json_for_storage().unwrap(),
            current.encode_json_for_storage().unwrap()
        );
    }
}

#[tokio::test]
async fn new_parent_input_preserves_running_child_and_original_permission_selector() {
    let db = database().await;
    super::input_sources::add_input_tables(&db).await;
    db.execute(
        Schema::new(db.get_database_backend())
            .create_table_from_entity(grant_row::Entity)
            .if_not_exists(),
    )
    .await
    .unwrap();
    let (parent, calls) = super::creation::runnable_parent(&db).await;
    let store = SubAgentStore::new(db.clone());
    let ordinary = store
        .spawn_for_turn(&parent, &calls[0], &super::creation::spawn_request())
        .await
        .unwrap();
    let run = load(&db, &ordinary.task_id).await;
    let SubAgentClaimOutcome::Claimed(ordinary_claim) = store
        .claim_child(
            &super::creation::claim_params(&run),
            &ordinary.task_id,
            run.fence(),
            &super::creation::destination(),
        )
        .await
        .unwrap()
    else {
        panic!("ordinary claim");
    };
    super::input_sources::release_parent(&db).await;
    super::input_sources::append_input(&db, true, "goal-with-original-permission").await;
    let task = super::input_sources::spawn_goal_child(&db, &store).await;
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
        panic!("goal claim");
    };
    let mut child = claimed.session;
    child.policy_revision =
        desk_diagnose_core::assistant_policy::PERSONAL_ASSISTANT_POLICY_REVISION;
    save_child_session(&db, &mut child).await.unwrap();
    let event = request(&mut child);
    publish(&db, &mut child, &event).await;
    child.finish_turn(TurnState::Idle, chrono::Utc::now().to_rfc3339());
    save_child_session(&db, &mut child).await.unwrap();
    super::input_sources::release_parent(&db).await;
    let ordinary_before = load(&db, &ordinary.task_id).await;
    let goal_before = load(&db, &task.task_id).await;
    let child_before = session(&db, &child.conversation_id)
        .await
        .encode_json_for_storage()
        .unwrap();
    super::input_sources::append_input(&db, false, "new-main-input-during-child-approval").await;
    assert_eq!(session(&db, "root").await.input_revision, 3);
    assert_eq!(load(&db, &ordinary.task_id).await, ordinary_before);
    assert_eq!(load(&db, &task.task_id).await, goal_before);
    assert_eq!(
        session(&db, &child.conversation_id)
            .await
            .encode_json_for_storage()
            .unwrap(),
        child_before
    );
    // The current parent and sibling selectors cannot approve the child's request.
    assert!(!decide_at(&db, "root").await);
    assert!(!decide_at(&db, &ordinary_claim.session.conversation_id).await);
    assert!(grant_row::Entity::find().all(&db).await.unwrap().is_empty());
    assert_eq!(
        session(&db, &child.conversation_id)
            .await
            .encode_json_for_storage()
            .unwrap(),
        child_before
    );
    approve(&db, &child.conversation_id).await;
    assert!(pending(&db, &child.conversation_id).await);
    let recorded = session(&db, &child.conversation_id).await;
    assert_eq!(
        recorded.permission_requests[0].state,
        PermissionRequestState::Approved
    );
    assert_eq!(recorded.input_revision, child.input_revision);
    assert_eq!(recorded.agent_role, child.agent_role);
    assert_eq!(grant_row::Entity::find().all(&db).await.unwrap().len(), 1);
    assert_claim(&db, &child.conversation_id, true).await;
    assert_eq!(load(&db, &ordinary.task_id).await, ordinary_before);
    assert_eq!(load(&db, &task.task_id).await.binding, goal_before.binding);
    assert_eq!(
        session(&db, &ordinary_claim.session.conversation_id)
            .await
            .agent_role,
        ordinary_claim.session.agent_role
    );
}
