//! AI review of a fresh-task proposal decides only the exact waiting proposal.
use super::*;
use crate::agent_approval_store as approval;
use crate::config::ConfigConnection;
use desk_diagnose_core::{
    approval_review::{ApprovalReviewDecision, ApprovalVerdict, PendingRequestKind},
    chat::{ChatMessage, ToolCall, ToolCallRef},
    session::TurnState,
};
use sea_orm::{PaginatorTrait, sea_query::Expr};

/// A running owner turn that just proposed a fresh task and waits for its decision.
async fn proposal_fixture() -> (DatabaseConnection, PersistedAgentSession, String, u64) {
    proposal_fixture_with(true).await
}

async fn proposal_fixture_with(
    owner_switch: bool,
) -> (DatabaseConnection, PersistedAgentSession, String, u64) {
    let db = database().await;
    let schema = Schema::new(db.get_database_backend());
    for mut table in [
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
    let user = desk_diagnose_core::model_message_labels::model_bound_user_message(
        "schedule-user".into(),
        "Check the device every morning".into(),
        destination,
    )
    .unwrap();
    let mut session = super::input_sources::first_input(&db, user.clone(), now).await;
    session.current_turn_id = Some("schedule-turn".into());
    session.lease_token = 1;
    if owner_switch {
        super::shared_approval::enable(&db, "schedule-delegation").await;
    }
    session.file_scope = Default::default();
    session.permission_requests.clear();
    let call = ToolCall {
        id: "schedule-call".into(),
        name: desk_diagnose_core::schedule::proposal::REQUEST_SCHEDULE.into(),
        arguments_json: serde_json::json!({"kind":"fresh_task","title":"Morning check",
            "prompt":"Prepare a read-only status report",
            "rule":{"kind":"daily","utc_time":"06:00:00"}})
        .to_string(),
    };
    let mut assistant = ChatMessage::assistant_tool_calls(
        "schedule-assistant",
        "",
        vec![ToolCallRef {
            id: call.id.clone(),
            name: call.name.clone(),
            arguments_json: call.arguments_json.clone(),
        }],
    );
    assistant.turn_id = session.current_turn_id.clone();
    let bytes = desk_diagnose_core::model_egress::message_content_bytes(&assistant).unwrap();
    assistant.data_envelope = Some(
        desk_diagnose_core::subagent::projection::envelope(
            "schedule-assistant",
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
        Default::default(),
    )
    .await
    .unwrap();
    txn.commit().await.unwrap();
    session_row::Entity::update_many()
        .col_expr(
            session_row::Column::StateJson,
            Expr::value(session.encode_json_for_storage().unwrap()),
        )
        .col_expr(
            session_row::Column::LeaseToken,
            Expr::value(session.lease_token as i64),
        )
        .col_expr(
            session_row::Column::LeaseDeadline,
            Expr::value(Some(now + chrono::Duration::minutes(5))),
        )
        .filter(session_row::Column::ConversationId.eq(&session.conversation_id))
        .exec(&db)
        .await
        .unwrap();
    let row = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq(&session.conversation_id))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    session = PersistedAgentSession::decode_json(&row.state_json).unwrap();
    crate::schedule_store::ScheduleStore::new(db.clone())
        .manage_from_session(
            &mut session,
            &call,
            &desk_diagnose_core::model_observability::tool::ToolObservation::default(),
        )
        .await
        .unwrap();
    let schedule_id = session.pending_schedule_review.clone().unwrap();
    (db, session, schedule_id, now.timestamp_millis() as u64)
}

async fn reviewed(
    db: &DatabaseConnection,
    session: &PersistedAgentSession,
    schedule_id: &str,
    verdict: ApprovalVerdict,
    now: u64,
) -> String {
    let pending = approval::pending_permission_reviews(db, 0, 64, now)
        .await
        .unwrap()
        .0;
    assert!(pending.iter().any(
        |item| item.kind == PendingRequestKind::Schedule && item.request_id == schedule_id
    ));
    let registry = desk_diagnose_core::ai_assistant::ai_assistant_provider_registry();
    let surface = desk_agent_protocol::capability_provider::ProductSurface::OssPersonalOwner;
    let candidate = approval::prepare_permission_review_batch(
        db,
        &session.conversation_id,
        "1",
        &session.device_id,
        schedule_id,
        PendingRequestKind::Schedule,
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
    let decision = ApprovalReviewDecision {
        candidate_id: candidate.candidate_id.clone(),
        verdict,
        reason_code: "owner_request".into(),
        reason: "The task matches the owner's morning check request".into(),
        evidence_event_ids: vec!["schedule-user".into()],
    };
    approval::settle_permission_review(
        db,
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
    .unwrap()
}

async fn saved(db: &DatabaseConnection, conversation: &str) -> PersistedAgentSession {
    let row = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq(conversation))
        .one(db)
        .await
        .unwrap()
        .unwrap();
    PersistedAgentSession::decode_json(&row.state_json).unwrap()
}

#[actix_web::test]
async fn ai_approval_enables_the_waiting_proposal_without_device_permissions() {
    let (db, session, schedule_id, now) = proposal_fixture().await;
    let status = reviewed(&db, &session, &schedule_id, ApprovalVerdict::Approve, now).await;
    assert_eq!(status, "approved");
    let task = crate::schedule_store::ScheduleStore::new(db.clone())
        .read(1, &schedule_id)
        .await
        .unwrap();
    assert_eq!(task.status, "active");
    assert!(task.next_run_at.is_some());
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
    assert_eq!(review.source_kind, "schedule_proposal");
    assert_eq!(review.source_id, schedule_id);
    // The waiting turn receives exactly one decision receipt naming the reviewer.
    let current = saved(&db, &session.conversation_id).await;
    assert_eq!(current.conversation.len(), session.conversation.len() + 1);
    let receipt: serde_json::Value =
        serde_json::from_str(&current.conversation.last().unwrap().text).unwrap();
    assert_eq!(receipt["event"], "scheduled_task_activated");
    assert_eq!(receipt["schedule_id"], schedule_id);
    assert!(
        current
            .conversation
            .last()
            .unwrap()
            .text
            .contains("morning check request")
    );
    // A decided proposal is no longer offered for review.
    assert!(
        !approval::pending_permission_reviews(&db, 0, 64, now + 2)
            .await
            .unwrap()
            .0
            .iter()
            .any(|item| item.kind == PendingRequestKind::Schedule)
    );
}

#[actix_web::test]
async fn ai_denial_rejects_the_exact_proposal() {
    let (db, session, schedule_id, now) = proposal_fixture().await;
    let status = reviewed(&db, &session, &schedule_id, ApprovalVerdict::Deny, now).await;
    assert_eq!(status, "denied");
    let task = crate::schedule_store::ScheduleStore::new(db.clone())
        .read(1, &schedule_id)
        .await
        .unwrap();
    assert_eq!(task.status, "deleted");
    let current = saved(&db, &session.conversation_id).await;
    let receipt: serde_json::Value =
        serde_json::from_str(&current.conversation.last().unwrap().text).unwrap();
    assert_eq!(receipt["event"], "scheduled_task_rejected");
}

#[actix_web::test]
async fn an_owner_decision_first_leaves_nothing_for_the_reviewer() {
    let (db, session, schedule_id, now) = proposal_fixture().await;
    let task = crate::schedule_store::ScheduleStore::new(db.clone())
        .read(1, &schedule_id)
        .await
        .unwrap();
    crate::schedule_store::ScheduleStore::new(db.clone())
        .decide_proposal(
            1,
            &schedule_id,
            task.revision,
            true,
            crate::schedule_store::ProposalDecider::Owner,
        )
        .await
        .unwrap();
    assert!(
        !approval::pending_permission_reviews(&db, 0, 64, now)
            .await
            .unwrap()
            .0
            .iter()
            .any(|item| item.kind == PendingRequestKind::Schedule)
    );
    assert!(
        approval::prepare_permission_review_batch(
            &db,
            &session.conversation_id,
            "1",
            &session.device_id,
            &schedule_id,
            PendingRequestKind::Schedule,
            &desk_diagnose_core::ai_assistant::ai_assistant_provider_registry(),
            desk_agent_protocol::capability_provider::ProductSurface::OssPersonalOwner,
            0,
            now,
        )
        .await
        .is_err()
    );
}

#[actix_web::test]
async fn a_proposal_without_review_opens_the_default_ai_review_in_its_turn() {
    let (db, session, schedule_id, now) = proposal_fixture_with(false).await;
    let delegation = crate::entity::agent_approval_delegation::Entity::find()
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(delegation.conversation_id, session.conversation_id);
    assert!(approval::decode(&delegation).unwrap().is_system_default());
    // The audit event and the proposal receipt were saved together.
    assert_eq!(saved(&db, &session.conversation_id).await, session);
    assert_eq!(
        reviewed(&db, &session, &schedule_id, ApprovalVerdict::Approve, now).await,
        "approved"
    );
}
