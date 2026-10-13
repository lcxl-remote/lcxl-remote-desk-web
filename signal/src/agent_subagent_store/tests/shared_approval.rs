//! Root approval covers existing and future children without copying grants.
use super::*;
use crate::agent_approval_store as approval;
use crate::config::ConfigConnection;
use crate::entity::agent_approval_delegation as ledger;
use desk_agent_protocol::data_lineage::Sensitivity;
use desk_diagnose_core::{
    approval_delegation::ApprovalDelegation,
    chat::{ChatMessage, ToolCallRef},
    session::TriggerOrigin,
};
use sea_orm::sea_query::Expr;

async fn enable(db: &DatabaseConnection, id: &str) -> ApprovalDelegation {
    let now = chrono::Utc::now().timestamp_millis();
    let value = ApprovalDelegation::new_usage_only(
        id.into(),
        "root".into(),
        "1".into(),
        "1".into(),
        "owner-switch".into(),
        now as u64,
    )
    .unwrap();
    ledger::ActiveModel {
        delegation_id: Set(value.delegation_id.clone()),
        conversation_id: Set("root".into()),
        actor_id: Set("1".into()),
        device_id: Set("1".into()),
        state_json: Set(serde_json::to_string(&value).unwrap()),
        status: Set("active".into()),
        version: Set(value.ledger_version as i64),
        created_at: Set(now),
        updated_at: Set(now),
        ..Default::default()
    }
    .insert(db)
    .await
    .unwrap();
    value
}

async fn child(
    db: &DatabaseConnection,
    parent: &PersistedAgentSession,
    call: &desk_diagnose_core::chat::ToolCall,
) -> PersistedAgentSession {
    let task = SubAgentStore::new(db.clone())
        .spawn_for_turn(parent, call, &super::creation::spawn_request())
        .await
        .unwrap();
    let run = load(db, &task.task_id).await;
    let mut params = super::creation::claim_params(&run);
    params.policy_revision = parent.policy_revision;
    let SubAgentClaimOutcome::Claimed(claimed) = SubAgentStore::new(db.clone())
        .claim_child(
            &params,
            &task.task_id,
            run.fence(),
            &super::creation::destination(),
        )
        .await
        .unwrap()
    else {
        panic!("child claim");
    };
    let mut session = claimed.session;
    session.trigger_origin = TriggerOrigin::DelegatedTask;
    let mut event = super::paused_permission::request(&mut session);
    event.request.request_id = format!("request-{}", task.task_id);
    event.event.event_id = format!("requested-{}", task.task_id);
    event.event.correlation_id = Some(event.request.request_id.clone());
    session.permission_requests[0] = event.request.clone();
    let source = session
        .delegated_owner_requirement
        .as_ref()
        .unwrap()
        .data_envelope
        .as_ref()
        .unwrap()
        .clone();
    let mut assistant = ChatMessage::assistant_tool_calls(
        format!("ask-{}", task.task_id),
        "Request child permission",
        vec![ToolCallRef {
            id: format!("ask-call-{}", task.task_id),
            name: "request_permissions".into(),
            arguments_json: "{}".into(),
        }],
    );
    let mut result = ChatMessage::tool_result(format!("result-{}", task.task_id), format!("ask-call-{}", task.task_id),
        serde_json::json!({"request_id": event.request.request_id, "status": "pending_user_decision"}).to_string());
    for message in [&mut assistant, &mut result] {
        message.data_envelope = Some(
            desk_diagnose_core::subagent::projection::envelope(
                &message.message_id,
                &String::from_utf8(
                    desk_diagnose_core::model_egress::message_content_bytes(message).unwrap(),
                )
                .unwrap(),
                "test-review",
                std::slice::from_ref(&source),
            )
            .unwrap(),
        );
    }
    session.conversation.extend([assistant, result]);
    super::paused_permission::publish(db, &mut session, &event).await;
    session.finish_turn(
        desk_diagnose_core::session::TurnState::Idle,
        chrono::Utc::now().to_rfc3339(),
    );
    save_child_session(db, &mut session).await.unwrap();
    session
}

#[tokio::test]
async fn root_switch_discovers_existing_and_future_children_while_main_is_running() {
    let db = database().await;
    let (parent, calls) = super::creation::runnable_parent_with_source(
        &db,
        super::creation::destination(),
        Sensitivity::Sensitive,
    )
    .await;
    let first = child(&db, &parent, &calls[0]).await;
    assert!(
        approval::pending_permission_reviews(
            &db,
            0,
            64,
            chrono::Utc::now().timestamp_millis() as u64
        )
        .await
        .unwrap()
        .0
        .is_empty()
    );
    let delegation = enable(&db, "root-review").await;
    let second = child(&db, &parent, &calls[1]).await;
    let pending = approval::pending_permission_reviews(
        &db,
        0,
        64,
        chrono::Utc::now().timestamp_millis() as u64,
    )
    .await
    .unwrap()
    .0;
    assert_eq!(pending.len(), 2);
    assert!(
        pending
            .iter()
            .any(|item| item.conversation_id == first.conversation_id)
    );
    assert!(
        pending
            .iter()
            .any(|item| item.conversation_id == second.conversation_id)
    );
    for child in [&first, &second] {
        let view = approval::load_latest_for_subject(&db, &child.conversation_id, "1", "1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(view.delegation_id, delegation.delegation_id);
        assert_eq!(view.conversation_id, "root");
        assert!(
            !child
                .scope_snapshot
                .granted
                .contains(&desk_agent_protocol::Capability::ShellExecConfirmed)
        );
    }
    assert_eq!(ledger::Entity::find().count(&db).await.unwrap(), 1);
    let mut closed = delegation.clone();
    closed
        .close(desk_diagnose_core::approval_delegation::ApprovalDelegationStatus::Closed)
        .unwrap();
    ledger::Entity::update_many()
        .set(ledger::ActiveModel {
            status: Set("closed".into()),
            state_json: Set(serde_json::to_string(&closed).unwrap()),
            version: Set(closed.ledger_version as i64),
            ..Default::default()
        })
        .filter(ledger::Column::DelegationId.eq(&delegation.delegation_id))
        .exec(&db)
        .await
        .unwrap();
    assert!(
        approval::pending_permission_reviews(
            &db,
            0,
            64,
            chrono::Utc::now().timestamp_millis() as u64
        )
        .await
        .unwrap()
        .0
        .is_empty()
    );
    assert_eq!(
        approval::load_latest_for_subject(&db, &first.conversation_id, "1", "1")
            .await
            .unwrap()
            .unwrap()
            .status,
        desk_diagnose_core::approval_delegation::ApprovalDelegationStatus::Closed
    );
    // Reopening uses one new root delegation, not a stale child copy.
    let reopened = enable(&db, "root-review-new").await;
    assert_eq!(
        approval::load_latest_for_subject(&db, &first.conversation_id, "1", "1")
            .await
            .unwrap()
            .unwrap()
            .delegation_id,
        reopened.delegation_id
    );
    assert_eq!(
        approval::pending_permission_reviews(
            &db,
            0,
            64,
            chrono::Utc::now().timestamp_millis() as u64
        )
        .await
        .unwrap()
        .0
        .len(),
        2
    );
}

#[tokio::test]
async fn shared_review_validates_the_frozen_child_relationship_and_open_source() {
    let db = database().await;
    let (parent, calls) = super::creation::runnable_parent_with_source(
        &db,
        super::creation::destination(),
        Sensitivity::Sensitive,
    )
    .await;
    let child = child(&db, &parent, &calls[0]).await;
    let now = chrono::Utc::now().timestamp_millis();
    assert!(shared_approval_current_on(&db, &child, now).await.unwrap());
    for invalid in 0..4 {
        let mut altered = child.clone();
        match invalid {
            0 => altered.actor_id = "other-owner".into(),
            1 => altered.device_id = "other-device".into(),
            2 => {
                let desk_diagnose_core::subagent::AgentRole::SubAgent { binding } =
                    &mut altered.agent_role
                else {
                    unreachable!()
                };
                binding.root_conversation_id = "other-root".into();
            }
            3 => {
                altered.delegated_owner_requirement.as_mut().unwrap().text =
                    "forged owner text".into()
            }
            _ => unreachable!(),
        }
        assert!(
            !shared_approval_current_on(&db, &altered, now)
                .await
                .is_ok_and(|current| current)
        );
    }
    let group = group_row::Entity::find()
        .filter(group_row::Column::GroupId.eq(child.delegation_group_id.as_ref().unwrap()))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let mut paused = decode_group(&group).unwrap();
    paused.source_admission = SourceAdmission::Paused;
    group_row::Entity::update_many()
        .col_expr(
            group_row::Column::StateJson,
            sea_orm::sea_query::Expr::value(serde_json::to_string(&paused).unwrap()),
        )
        .col_expr(
            group_row::Column::SourceAdmission,
            sea_orm::sea_query::Expr::value("paused"),
        )
        .filter(group_row::Column::Id.eq(group.id))
        .exec(&db)
        .await
        .unwrap();
    assert!(!shared_approval_current_on(&db, &child, now).await.unwrap());
}

async fn review_child_fixture() -> (
    DatabaseConnection,
    PersistedAgentSession,
    ApprovalDelegation,
) {
    let db = database().await;
    let schema = Schema::new(db.get_database_backend());
    for mut table in [
        schema.create_table_from_entity(crate::entity::approval_review_secret::Entity),
        schema.create_table_from_entity(crate::entity::approval_model_probe_observation::Entity),
        schema.create_table_from_entity(crate::entity::agent_capability_grant::Entity),
    ] {
        db.execute(table.if_not_exists()).await.unwrap();
    }
    let (parent, calls) = super::creation::runnable_parent_with_source(
        &db,
        super::creation::destination(),
        Sensitivity::Sensitive,
    )
    .await;
    let child = child(&db, &parent, &calls[0]).await;
    db.config_context()
        .update::<_, DbErr, _>(|file| {
            file.approval_gateway.enabled = true;
            file.approval_gateway.gateway.base_url = Some("http://127.0.0.1:1".into());
            file.approval_gateway.gateway.model = Some("test-reviewer".into());
            file.approval_gateway.gateway.api_key = Some("test-key".into());
            file.approval_gateway.gateway.wire_protocol =
                Some(desk_diagnose_core::model_profile::WireProtocol::OpenAiChatCompletions);
            file.approval_gateway.gateway.output_limit_field =
                desk_diagnose_core::model_profile::OutputLimitField::MaxTokens;
            Ok(Some(()))
        })
        .await
        .unwrap();
    let delegation = enable(&db, "shared-claim-review").await;
    (db, child, delegation)
}

#[actix_web::test]
async fn active_child_directory_is_claimed_once_and_fault_ends_only_directory_consent() {
    use desk_agent_protocol::capability_provider::ProductSurface;
    use desk_diagnose_core::file_scope::transaction::{self, FileScopeMutation, FileScopeUpdate};
    let (db, mut session, _) = review_child_fixture().await;
    let now = chrono::Utc::now();
    session.turn_state = desk_diagnose_core::session::TurnState::Running;
    session.current_turn_id = Some("directory-child-turn".into());
    let id = desk_diagnose_core::native_ids::stable(
        desk_diagnose_core::native_ids::Kind::DirectoryRequest,
        &(
            "directory-proposal",
            &(
                &session.conversation_id,
                session.input_revision,
                "directory-call",
            ),
        ),
    );
    let source = session
        .authorization_requirement()
        .unwrap()
        .data_envelope
        .clone()
        .unwrap();
    let mut assistant = ChatMessage::assistant_tool_calls(
        "directory-assistant",
        "",
        vec![ToolCallRef {
            id: "directory-call".into(),
            name: "request_directory".into(),
            arguments_json: r#"{"path":"/work","purpose":"report"}"#.into(),
        }],
    );
    assistant.turn_id = session.current_turn_id.clone();
    let bytes = desk_diagnose_core::model_egress::message_content_bytes(&assistant).unwrap();
    assistant.data_envelope = Some(
        desk_diagnose_core::subagent::projection::envelope(
            "directory-assistant",
            &String::from_utf8(bytes).unwrap(),
            "test",
            &[source],
        )
        .unwrap(),
    );
    session.conversation.push(assistant);
    let update = FileScopeUpdate {
        subject: session
            .file_scope_subject(
                &session.actor_id,
                &session.device_id,
                &session.conversation_id,
            )
            .unwrap(),
        client_conversation_id: session.file_scope_selector().unwrap().into(),
        client_request_id: id.clone(),
        expected_revision: 0,
        mutation: FileScopeMutation::Propose {
            proposal: desk_diagnose_core::file_scope::DirectoryProposal {
                request_id: id.clone(),
                requested_path: "/work".into(),
                canonical_path: "/work".into(),
                purpose: "report".into(),
                source: desk_diagnose_core::file_scope::DirectoryConsentSource::ModelProposal,
                directory: desk_agent_protocol::computer_use::ObjectRef {
                    token: "native-directory".into(),
                    snapshot_id: "generation".into(),
                    object_kind: desk_agent_protocol::computer_use::ObjectKind::Directory,
                    expires_at: (now + chrono::Duration::hours(1)).to_rfc3339(),
                },
            },
        },
    };
    session = transaction::prepare(&session, &update, now.timestamp_millis() as u64)
        .unwrap()
        .0;
    session_row::Entity::update_many()
        .col_expr(
            session_row::Column::StateJson,
            Expr::value(session.encode_json_for_storage().unwrap()),
        )
        .col_expr(session_row::Column::Version, Expr::value(session.version))
        .col_expr(
            session_row::Column::LeaseDeadline,
            Expr::value(now + chrono::Duration::minutes(10)),
        )
        .filter(session_row::Column::ConversationId.eq(&session.conversation_id))
        .exec(&db)
        .await
        .unwrap();
    let pending = approval::pending_permission_reviews(&db, 0, 64, now.timestamp_millis() as u64)
        .await
        .unwrap()
        .0;
    assert!(pending.iter().any(|review| review.directory_request
        && review.request_id == id
        && review.conversation_id == session.conversation_id));
    let registry = desk_diagnose_core::ai_assistant::ai_assistant_provider_registry();
    let candidate = approval::prepare_permission_review_batch(
        &db,
        &session.conversation_id,
        "1",
        "1",
        &id,
        true,
        &registry,
        ProductSurface::OssPersonalOwner,
        1,
        now.timestamp_millis() as u64,
    )
    .await
    .unwrap()
    .remove(0);
    let claim = approval::claim_permission_review(
        &db,
        &candidate,
        &registry,
        ProductSurface::OssPersonalOwner,
        "node-a",
        1,
        now.timestamp_millis() as u64,
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        approval::claim_permission_review(
            &db,
            &candidate,
            &registry,
            ProductSurface::OssPersonalOwner,
            "node-b",
            1,
            now.timestamp_millis() as u64
        )
        .await
        .unwrap()
        .is_none()
    );
    approval::deny_directory_unavailable(
        &db,
        &session.conversation_id,
        "1",
        "1",
        &id,
        now.timestamp_millis() as u64,
        false,
    )
    .await
    .unwrap();
    approval::settle_permission_review(
        &db,
        &candidate,
        &claim.lease_owner,
        claim.lease_epoch,
        None,
        None,
        1,
        now.timestamp_millis() as u64 + 1,
        None,
    )
    .await
    .unwrap();
    let row = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq(&session.conversation_id))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    let current = PersistedAgentSession::decode_json(&row.state_json).unwrap();
    assert!(current.scope_snapshot.granted.is_empty());
    assert_eq!(current.permission_requests, session.permission_requests);
    let mut held = session;
    assert_eq!(
        desk_diagnose_core::directory_tools::adopt_review_snapshot(&mut held, current, &id)
            .unwrap(),
        Some(false)
    );
    assert_eq!(
        held.file_scope.records()[0]
            .decision
            .as_ref()
            .unwrap()
            .candidate_id
            .as_deref(),
        Some(candidate.candidate_id.as_str())
    );
}

#[actix_web::test]
async fn closed_child_claim_releases_the_root_reservation_without_dialing() {
    use desk_agent_protocol::capability_provider::ProductSurface;
    let (db, child, mut delegation) = review_child_fixture().await;
    let registry = desk_diagnose_core::ai_assistant::ai_assistant_provider_registry();
    let now = chrono::Utc::now().timestamp_millis() as u64;
    let candidates = approval::prepare_permission_review_batch(
        &db,
        &child.conversation_id,
        "1",
        "1",
        &child.permission_requests[0].request_id,
        false,
        &registry,
        ProductSurface::OssPersonalOwner,
        1,
        now,
    )
    .await
    .unwrap();
    let candidate = &candidates[0];
    assert_eq!(candidate.delegation_id, delegation.delegation_id);
    assert_eq!(candidate.conversation_id, child.conversation_id);
    assert_eq!(
        candidate.context.current_user_requirement,
        child.delegated_owner_requirement.as_ref().unwrap().text
    );
    let claim = approval::claim_permission_review(
        &db,
        candidate,
        &registry,
        ProductSurface::OssPersonalOwner,
        "child-review-worker",
        1,
        now,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(claim.call_authority.root_conversation_id, "root");
    assert_eq!(claim.call_authority.conversation_id, child.conversation_id);
    assert!(claim.delegation_call.is_some());
    assert!(
        approval::claim_permission_review(
            &db,
            candidate,
            &registry,
            ProductSurface::OssPersonalOwner,
            "competing-worker",
            1,
            now
        )
        .await
        .unwrap()
        .is_none()
    );
    delegation = approval::load_latest_for_subject(&db, "root", "1", "1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(delegation.usage.reviews_reserved, 1);
    delegation
        .close(desk_diagnose_core::approval_delegation::ApprovalDelegationStatus::Closed)
        .unwrap();
    ledger::Entity::update_many()
        .set(ledger::ActiveModel {
            state_json: Set(serde_json::to_string(&delegation).unwrap()),
            version: Set(delegation.ledger_version as i64),
            status: Set("closed".into()),
            ..Default::default()
        })
        .filter(ledger::Column::DelegationId.eq(&delegation.delegation_id))
        .exec(&db)
        .await
        .unwrap();
    let txn = db.begin().await.unwrap();
    assert!(
        mark_review_dispatch_on(
            &txn,
            &claim.call_authority,
            "oss_model_egress",
            "blocked-provider-call",
            now as i64
        )
        .await
        .is_err()
    );
    txn.rollback().await.unwrap();
    assert_eq!(
        crate::entity::model_egress_receipt::Entity::find()
            .count(&db)
            .await
            .unwrap(),
        0
    );
    let after_lease = claim.lease_deadline_unix_ms + 1;
    assert_eq!(
        approval::expire_review_leases(&db, after_lease, 64)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        approval::expire_review_leases(&db, after_lease, 64)
            .await
            .unwrap(),
        0
    );
    let closed = approval::load_latest_for_subject(&db, "root", "1", "1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(closed.usage.reviews_reserved, 0);
    assert_eq!(closed.usage.tokens_reserved, 0);
    assert_eq!(closed.usage.reviews_used, 0);
    assert_eq!(
        closed.status,
        desk_diagnose_core::approval_delegation::ApprovalDelegationStatus::Closed
    );
}

#[tokio::test]
async fn approved_child_review_keeps_its_grant_and_audit_in_the_child_and_usage_in_the_root() {
    use crate::model_egress_store::SignalModelEgressStore;
    use desk_agent_protocol::capability_provider::ProductSurface;
    use desk_diagnose_core::approval_review::{ApprovalReviewDecision, ApprovalVerdict};
    let (db, child, delegation) = review_child_fixture().await;
    let registry = desk_diagnose_core::ai_assistant::ai_assistant_provider_registry();
    let now = chrono::Utc::now().timestamp_millis() as u64;
    let request_id = &child.permission_requests[0].request_id;
    let candidates = approval::prepare_permission_review_batch(
        &db,
        &child.conversation_id,
        "1",
        "1",
        request_id,
        false,
        &registry,
        ProductSurface::OssPersonalOwner,
        1,
        now,
    )
    .await
    .unwrap();
    let candidate = &candidates[0];
    let claim = approval::claim_permission_review(
        &db,
        candidate,
        &registry,
        ProductSurface::OssPersonalOwner,
        "approved-child-worker",
        1,
        now,
    )
    .await
    .unwrap()
    .unwrap();
    // A synthetic provider receipt tests the durable decision boundary without
    // sending a model request or executing anything on the real device.
    let receipt_id = "isolated-child-model-call";
    let txn = db.begin().await.unwrap();
    SubAgentStore::link_model_receipt_on(
        &txn,
        claim.delegation_call.as_ref().unwrap(),
        receipt_id,
        now as i64,
    )
    .await
    .unwrap();
    SignalModelEgressStore::record_dispatch_intent_on(
        &txn,
        receipt_id.into(),
        "isolated-export".into(),
        1,
        &claim.authorized.prompt_audit,
        std::slice::from_ref(&claim.authorized.prompt_envelope),
    )
    .await
    .unwrap();
    txn.commit().await.unwrap();
    SignalModelEgressStore::new(db.clone())
        .record_terminal_usage(
            receipt_id,
            &desk_diagnose_core::chat::TokenUsage {
                input_tokens: Some(100),
                output_tokens: Some(20),
                cache_read_tokens: Some(0),
                cache_write_tokens: Some(0),
            },
            None,
        )
        .await
        .unwrap();
    let decision = ApprovalReviewDecision {
        candidate_id: candidate.candidate_id.clone(),
        verdict: ApprovalVerdict::Approve,
        reason_code: "within_scope".into(),
        reason: "The original owner asked for this inspection".into(),
        evidence_event_ids: vec![
            child
                .delegated_owner_requirement
                .as_ref()
                .unwrap()
                .message_id
                .clone(),
        ],
    };
    assert_eq!(
        approval::settle_permission_review(
            &db,
            candidate,
            &claim.lease_owner,
            claim.lease_epoch,
            Some(&decision),
            Some(120),
            1,
            chrono::Utc::now().timestamp_millis() as u64,
            None,
        )
        .await
        .unwrap(),
        "approved"
    );
    let inventory = vec![
        desk_diagnose_core::capability_availability::CapabilityAvailability {
            provider_id: "desktop.session".into(),
            capability_id: "desktop.session.inspect".into(),
            tool_name: "inspect_desktop_session".into(),
            compiled: true,
            enabled: true,
            connected: true,
            ready: true,
            reason: None,
        },
    ];
    crate::agent_session_store::SignalAgentSessionStore::new(db.clone())
        .decide_permission_request_by_ai(
            crate::agent_session_store::PermissionDecisionSubject {
                conversation_id: &child.conversation_id,
                actor_id: "1",
                device_id: "1",
            },
            request_id,
            &candidates,
            crate::agent_session_store::PermissionGrantIssuanceContext {
                surface: ProductSurface::OssPersonalOwner,
                registry: &registry,
                inventory: &inventory,
                readiness_revision: 1,
                now_unix_ms: chrono::Utc::now().timestamp_millis() as u64,
                implicit_fresh_object_refs: &[],
            },
            &chrono::Utc::now().to_rfc3339(),
        )
        .await
        .unwrap();
    let child = super::paused_permission::session(&db, &child.conversation_id).await;
    let grants = crate::capability_grant_store::SignalCapabilityGrantStore::new(db.clone())
        .list_for_subject(&child.conversation_id, "1", "1")
        .await
        .unwrap();
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0].run_id, child.conversation_id);
    let txn = db.begin().await.unwrap();
    assert_eq!(
        approval::current_grant_parent_on(
            &txn,
            &child,
            &grants[0],
            chrono::Utc::now().timestamp_millis() as u64
        )
        .await
        .unwrap()
        .delegation_id,
        delegation.delegation_id
    );
    txn.rollback().await.unwrap();
    assert!(
        crate::capability_grant_store::SignalCapabilityGrantStore::new(db.clone())
            .list_for_subject("root", "1", "1")
            .await
            .unwrap()
            .is_empty()
    );
    let total = approval::load_latest_for_subject(&db, "root", "1", "1")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(total.usage.reviews_used, 1);
    assert_eq!(total.usage.tokens_used, 120);
    assert_eq!(total.usage.reviews_reserved, 0);
    assert_eq!(
        child.permission_requests[0].state,
        desk_diagnose_core::dynamic_run::PermissionRequestState::Approved
    );
}
