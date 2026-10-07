//! Goal proposals must use the durable role even if a caller spoofs its copy.
use super::*;
use desk_diagnose_core::{
    dynamic_run::AgentRunEventKind,
    goal::{GoalLimits, GoalModelBinding, GoalOpenRequest, GoalOpenRequestEvent},
    session::TriggerOrigin,
    subagent::AgentRole,
};

pub(super) fn proposal(session: &mut PersistedAgentSession) -> GoalOpenRequestEvent {
    let now = chrono::Utc::now();
    let source =
        desk_diagnose_core::permission_resume::latest_user_requirement(&session.conversation)
            .map(|message| message.message_id.clone())
            .unwrap_or_else(|| "forged-source".into());
    let request = GoalOpenRequest::new(
        format!("goal-open-{}", session.conversation_id),
        session.conversation_id.clone(),
        session.actor_id.clone(),
        session.device_id.clone(),
        source,
        session.input_revision,
        "Complete this bounded investigation".into(),
        GoalLimits::default(),
        GoalModelBinding::from_destination(&super::creation::destination()).unwrap(),
        now.timestamp_millis() as u64,
    )
    .unwrap();
    session.last_event_seq += 1;
    GoalOpenRequestEvent::new(
        &request,
        AgentRunEventKind::GoalOpenRequested,
        session.last_event_seq,
        now.to_rfc3339(),
    )
    .unwrap()
}

#[tokio::test]
async fn durable_child_role_rejects_forged_main_goal_proposal_without_writes() {
    let db = database().await;
    super::input_sources::add_input_tables(&db).await;
    let original = super::lifecycle::child(&db).await;
    // Decode the persisted copy, as recovery does; model history is not role authority.
    let before = super::paused_permission::session(&db, &original.conversation_id).await;
    let mut forged =
        PersistedAgentSession::decode_json(&before.encode_json_for_storage().unwrap()).unwrap();
    forged.agent_role = AgentRole::Main;
    forged.trigger_origin = TriggerOrigin::User;
    let event = proposal(&mut forged);
    let error = goal_open::save_model_request(&db, &mut forged, &event)
        .await
        .unwrap_err();
    assert_eq!(error.message, "stored child session cannot propose a goal");
    assert_eq!(
        super::paused_permission::session(&db, &original.conversation_id)
            .await
            .encode_json_for_storage()
            .unwrap(),
        before.encode_json_for_storage().unwrap()
    );
    assert!(open_row::Entity::find().all(&db).await.unwrap().is_empty());
    assert!(goal_row::Entity::find().all(&db).await.unwrap().is_empty());
    // The same boundary still accepts an actual main session's proposal.
    let mut parent = super::paused_permission::session(&db, "root").await;
    parent.trigger_origin = TriggerOrigin::User;
    let event = proposal(&mut parent);
    goal_open::save_model_request(&db, &mut parent, &event)
        .await
        .unwrap();
    let proposals = open_row::Entity::find().all(&db).await.unwrap();
    assert_eq!(proposals.len(), 1);
    assert_eq!(proposals[0].conversation_id, "root");
    assert_eq!(
        super::paused_permission::session(&db, &original.conversation_id)
            .await
            .encode_json_for_storage()
            .unwrap(),
        before.encode_json_for_storage().unwrap()
    );
}

use crate::agent_goal_open_store as goal_open;
use crate::entity::agent_goal_open_request as open_row;
