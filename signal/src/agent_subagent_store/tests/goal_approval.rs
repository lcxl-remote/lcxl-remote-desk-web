//! Goal review follows the durable approval transaction without creating device grants.
use super::*;
use crate::agent_approval_store as approval;
use crate::config::ConfigConnection;
use desk_diagnose_core::{
    approval_review::{
        ApprovalReviewCandidate, ApprovalReviewDecision, ApprovalVerdict, PendingRequestKind,
    },
    chat::{ChatMessage, ToolCallRef},
    goal::{GoalLimits, GoalModelBinding, GoalOpenRequest},
    session::TurnState,
};
use sea_orm::{PaginatorTrait, sea_query::Expr};
async fn fixture() -> (
    DatabaseConnection,
    PersistedAgentSession,
    GoalOpenRequest,
    u64,
) {
    let db = database().await;
    let schema = Schema::new(db.get_database_backend());
    for mut table in [
        schema.create_table_from_entity(crate::entity::agent_goal_open_request::Entity),
        schema.create_table_from_entity(crate::entity::approval_review_secret::Entity),
        schema.create_table_from_entity(crate::entity::approval_model_probe_observation::Entity),
        schema.create_table_from_entity(crate::entity::model_probe_observation::Entity),
        schema.create_table_from_entity(crate::entity::agent_capability_grant::Entity),
    ] {
        db.execute(table.if_not_exists()).await.unwrap();
    }
    db.config_context()
        .update::<_, DbErr, _>(|file| {
            file.ai_gateway.base_url = Some("http://127.0.0.1:1".into());
            file.ai_gateway.max_context_bytes = Some(1_048_576);
            file.ai_gateway.model = Some("test-agent".into());
            file.ai_gateway.api_key = Some("test-key".into());
            file.ai_gateway.wire_protocol =
                Some(desk_diagnose_core::model_profile::WireProtocol::OpenAiChatCompletions);
            file.ai_gateway.output_limit_field =
                desk_diagnose_core::model_profile::OutputLimitField::MaxTokens;
            file.approval_gateway.enabled = true;
            file.approval_gateway.gateway = file.ai_gateway.clone();
            Ok(Some(()))
        })
        .await
        .unwrap();
    let destination = crate::model_provider::load(&db)
        .await
        .unwrap()
        .destination_identity()
        .unwrap();
    let now = chrono::Utc::now();
    let mut session = PersistedAgentSession::new("root", "1", "1", 1, scope(), now.to_rfc3339());
    session.adopt_client_metadata(Some("client"), AgentSessionSurface::AiAssistant);
    session.input_revision = 1;
    session.begin_focus_epoch(1, Vec::new()).unwrap();
    session.control_revision = 1;
    session.latest_input_seq = 1;
    session.current_turn_id = Some("goal-turn".into());
    session.lease_token = 1;
    insert_session(&db, &session).await;
    super::shared_approval::enable(&db, "goal-delegation").await;
    session.turn_state = TurnState::Idle;
    session.file_scope = Default::default();
    session.conversation.clear();
    session.permission_requests.clear();
    let user = desk_diagnose_core::model_message_labels::model_bound_user_message(
        "goal-user".into(),
        "Please keep working until the report is finished".into(),
        destination.clone(),
    )
    .unwrap();
    let mut assistant = ChatMessage::assistant_tool_calls(
        "goal-assistant",
        "",
        vec![ToolCallRef {
            id: "goal-call".into(),
            name: "request_goal".into(),
            arguments_json: r#"{"goal_text":"Finish the report"}"#.into(),
        }],
    );
    assistant.turn_id = session.current_turn_id.clone();
    let bytes = desk_diagnose_core::model_egress::message_content_bytes(&assistant).unwrap();
    assistant.data_envelope = Some(
        desk_diagnose_core::subagent::projection::envelope(
            "goal-assistant",
            &String::from_utf8(bytes).unwrap(),
            "test",
            &[user.data_envelope.clone().unwrap()],
        )
        .unwrap(),
    );
    session.conversation = vec![user.clone(), assistant];
    session.turn_state = TurnState::Running;
    let txn = db.begin().await.unwrap();
    crate::agent_subagent_store::initialize_input_group_on(
        &txn,
        &mut session,
        user,
        None,
        None,
        now.timestamp_millis(),
    )
    .await
    .unwrap();
    txn.commit().await.unwrap();
    session.turn_state = TurnState::Idle;
    PersistedAgentSession::decode_json(&session.encode_json_for_storage().unwrap()).unwrap();
    session_row::Entity::update_many()
        .col_expr(
            session_row::Column::StateJson,
            Expr::value(session.encode_json_for_storage().unwrap()),
        )
        .col_expr(
            session_row::Column::LeaseDeadline,
            Expr::value(Option::<chrono::NaiveDateTime>::None),
        )
        .filter(session_row::Column::ConversationId.eq(&session.conversation_id))
        .exec(&db)
        .await
        .unwrap();
    let request = GoalOpenRequest::new(
        "goal-request".into(),
        session.conversation_id.clone(),
        session.actor_id.clone(),
        session.device_id.clone(),
        "goal-user".into(),
        session.input_revision,
        "Finish the report".into(),
        GoalLimits::default(),
        GoalModelBinding::from_destination(&destination).unwrap(),
        now.timestamp_millis() as u64,
    )
    .unwrap();
    let txn = db.begin().await.unwrap();
    crate::agent_goal_open_store::insert_on(&txn, &request)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    (db, session, request, now.timestamp_millis() as u64)
}

async fn claim(
    db: &DatabaseConnection,
    session: &PersistedAgentSession,
    request: &GoalOpenRequest,
    now: u64,
) -> (ApprovalReviewCandidate, approval::ClaimedPermissionReview) {
    let pending = approval::pending_permission_reviews(db, 0, 64, now)
        .await
        .unwrap()
        .0;
    assert!(
        pending
            .iter()
            .any(|item| item.kind == PendingRequestKind::Goal
                && item.request_id == request.request_id)
    );
    let registry = desk_diagnose_core::ai_assistant::ai_assistant_provider_registry();
    let surface = desk_agent_protocol::capability_provider::ProductSurface::OssPersonalOwner;
    let candidate = approval::prepare_permission_review_batch(
        db,
        &session.conversation_id,
        "1",
        &session.device_id,
        &request.request_id,
        PendingRequestKind::Goal,
        &registry,
        surface,
        0,
        now,
    )
    .await
    .unwrap()
    .remove(0);
    let claim =
        approval::claim_permission_review(db, &candidate, &registry, surface, "node-a", 0, now)
            .await
            .unwrap()
            .unwrap();
    assert!(
        approval::claim_permission_review(db, &candidate, &registry, surface, "node-b", 0, now)
            .await
            .unwrap()
            .is_none()
    );
    // Simulate the provider-start receipt without making a network call.
    let txn = db.begin().await.unwrap();
    crate::agent_subagent_store::SubAgentStore::link_model_receipt_on(
        &txn,
        claim
            .delegation_call
            .as_ref()
            .expect("input-group review allocation"),
        "test-review-call",
        now as i64,
    )
    .await
    .unwrap();
    txn.commit().await.unwrap();
    (candidate, claim)
}

#[actix_web::test]
async fn goal_approval_queues_once_without_device_permissions() {
    let (db, session, request, now) = fixture().await;
    let (candidate, claim) = claim(&db, &session, &request, now).await;
    let decision = ApprovalReviewDecision {
        candidate_id: candidate.candidate_id.clone(),
        verdict: ApprovalVerdict::Approve,
        reason_code: "owner_request".into(),
        reason: "The goal matches the owner's report request".into(),
        evidence_event_ids: vec!["goal-user".into()],
    };
    approval::settle_permission_review(
        &db,
        &candidate,
        &claim.lease_owner,
        claim.lease_epoch,
        Some(&decision),
        None,
        0,
        now + 1,
        None,
    )
    .await
    .unwrap();
    let stored = crate::agent_goal_open_store::pending_for_subject(
        &db,
        &session.conversation_id,
        "1",
        &session.device_id,
    )
    .await
    .unwrap();
    assert!(stored.is_none());
    let row = crate::entity::agent_goal_open_request::Entity::find()
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.status, "approved");
    let goal = crate::entity::agent_goal_run::Entity::find()
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(goal.status, "queued");
    assert_eq!(
        crate::entity::agent_capability_grant::Entity::find()
            .count(&db)
            .await
            .unwrap(),
        0
    );
    let review = crate::entity::agent_approval_review::Entity::find()
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(review.status, "approved");
    assert_eq!(review.source_kind, "goal_proposal");
    let event = crate::entity::agent_run_event::Entity::find()
        .filter(crate::entity::agent_run_event::Column::Kind.eq("goal_open_decided"))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let decision: desk_diagnose_core::goal::GoalOpenRequestEvent =
        serde_json::from_str(&event.payload_json).unwrap();
    decision.validate().unwrap();
    assert!(matches!(
        decision.review_decision.unwrap().source,
        desk_diagnose_core::dynamic_run::PermissionDecisionSource::AutomaticReview { .. }
    ));

    let row = session_row::Entity::find().one(&db).await.unwrap().unwrap();
    let current = PersistedAgentSession::decode_json(&row.state_json).unwrap();
    assert!(
        current
            .conversation
            .iter()
            .any(|message| message.text.contains("goal_proposal_decision"))
    );
    let closed = crate::agent_goal_open_store::decide_for_subject(
        &db,
        &session.conversation_id,
        "1",
        &session.device_id,
        &request.request_id,
        true,
        chrono::DateTime::from_timestamp_millis((now + 2) as i64).unwrap(),
    )
    .await
    .unwrap();
    assert!(closed.is_none());
}

#[actix_web::test]
async fn goal_review_fault_and_unavailable_configuration_close_the_exact_proposal() {
    for scenario in 0..4 {
        let (db, session, request, now) = fixture().await;
        if scenario != 1 {
            let (candidate, claim) = claim(&db, &session, &request, now).await;
            if scenario == 3 {
                assert_eq!(
                    approval::expire_review_leases(&db, claim.lease_deadline_unix_ms + 1, 64)
                        .await
                        .unwrap(),
                    1
                );
            } else {
                let denial = (scenario == 2).then(|| ApprovalReviewDecision {
                    candidate_id: candidate.candidate_id.clone(),
                    verdict: ApprovalVerdict::Deny,
                    reason_code: "scope".into(),
                    reason: "Outside the owner request".into(),
                    evidence_event_ids: vec!["goal-user".into()],
                });
                approval::settle_permission_review(
                    &db,
                    &candidate,
                    &claim.lease_owner,
                    claim.lease_epoch,
                    denial.as_ref(),
                    None,
                    0,
                    now + 1,
                    None,
                )
                .await
                .unwrap();
            }
        } else {
            assert!(
                approval::deny_goal_unavailable(
                    &db,
                    &session.conversation_id,
                    "1",
                    &session.device_id,
                    &request.request_id,
                    now + 1,
                    desk_diagnose_core::approval_observation::ReviewReason::Configuration
                )
                .await
                .unwrap()
            );
        }
        let row = crate::entity::agent_goal_open_request::Entity::find()
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, "denied");
        assert_eq!(
            crate::entity::agent_goal_run::Entity::find()
                .count(&db)
                .await
                .unwrap(),
            0
        );
        assert!(
            approval::pending_permission_reviews(&db, 0, 64, now + 2)
                .await
                .unwrap()
                .0
                .is_empty()
        );
    }
}

#[actix_web::test]
async fn human_decision_wins_over_a_late_ai_approval() {
    let (db, session, request, now) = fixture().await;
    let (candidate, claim) = claim(&db, &session, &request, now).await;
    crate::agent_goal_open_store::decide_for_subject(
        &db,
        &session.conversation_id,
        "1",
        &session.device_id,
        &request.request_id,
        false,
        chrono::DateTime::from_timestamp_millis((now + 1) as i64).unwrap(),
    )
    .await
    .unwrap()
    .unwrap();
    let decision = ApprovalReviewDecision {
        candidate_id: candidate.candidate_id.clone(),
        verdict: ApprovalVerdict::Approve,
        reason_code: "owner_request".into(),
        reason: "Matches".into(),
        evidence_event_ids: vec!["goal-user".into()],
    };
    approval::settle_permission_review(
        &db,
        &candidate,
        &claim.lease_owner,
        claim.lease_epoch,
        Some(&decision),
        None,
        0,
        now + 2,
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        crate::entity::agent_goal_open_request::Entity::find()
            .one(&db)
            .await
            .unwrap()
            .unwrap()
            .status,
        "denied"
    );
    assert_eq!(
        crate::entity::agent_goal_run::Entity::find()
            .count(&db)
            .await
            .unwrap(),
        0
    );
}

#[actix_web::test]
async fn closing_automatic_approval_returns_the_proposal_to_manual_review() {
    let (db, session, request, now) = fixture().await;
    let (candidate, claim) = claim(&db, &session, &request, now).await;
    approval::close_for_subject(
        &db,
        &session.conversation_id,
        "1",
        &session.device_id,
        &candidate.delegation_id,
        "off",
        now + 1,
    )
    .await
    .unwrap();
    let decision = ApprovalReviewDecision {
        candidate_id: candidate.candidate_id.clone(),
        verdict: ApprovalVerdict::Approve,
        reason_code: "owner_request".into(),
        reason: "Matches".into(),
        evidence_event_ids: vec!["goal-user".into()],
    };
    approval::settle_permission_review(
        &db,
        &candidate,
        &claim.lease_owner,
        claim.lease_epoch,
        Some(&decision),
        None,
        0,
        now + 2,
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        crate::entity::agent_goal_open_request::Entity::find()
            .one(&db)
            .await
            .unwrap()
            .unwrap()
            .status,
        "pending"
    );
    assert_eq!(
        crate::entity::agent_goal_run::Entity::find()
            .count(&db)
            .await
            .unwrap(),
        0
    );
    assert!(
        approval::pending_permission_reviews(&db, 0, 64, now + 3)
            .await
            .unwrap()
            .0
            .is_empty()
    );
}
