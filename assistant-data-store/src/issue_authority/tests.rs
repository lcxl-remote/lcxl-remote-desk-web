use super::*;
use desk_diagnose_core::{
    external_tools::IssueTarget,
    issue_authority::{IssueFilter, IssueRepositoryScope, IssueWorkKind},
    native_ids::{self, Kind},
};
use sea_orm::{Database, DatabaseConnection, Schema, SchemaBuilder, TransactionTrait};

async fn setup() -> DatabaseConnection {
    let db = Database::connect("sqlite::memory:").await.unwrap();
    crate::entity::register_schema(SchemaBuilder::new(Schema::new(db.get_database_backend())))
        .sync(&db)
        .await
        .unwrap();
    db
}
fn policy() -> IssueSubscription {
    IssueSubscription {
        subscription_id: native_ids::stable(Kind::IssueSubscription, &"source"),
        revision: 99,
        actor_id: "owner".into(),
        conversation_id: "principal".into(),
        enabled: true,
        repository: IssueRepositoryScope {
            platform: desk_agent_protocol::external_tools::IssuePlatform::Gitea,
            connection_ref: "forge".into(),
            instance: "https://forge.test/".into(),
            account_id: 3,
            repository_id: 42,
            owner: "org".into(),
            repository: "project".into(),
            private: true,
            internal: false,
        },
        issue_filter: None,
        allowed_sender_ids: vec![12, 7, 7],
        automation_sender_ids: vec![9],
        work_kind: IssueWorkKind::Analysis,
        auto_reply: true,
    }
}
fn admitted(policy: IssueSubscription) -> IssueAdmission {
    let issue = IssueAccess {
        connection_ref: "forge".into(),
        target: IssueTarget {
            platform: desk_agent_protocol::external_tools::IssuePlatform::Gitea,
            instance: "https://forge.test/".into(),
            account_id: 3,
            repository_id: 42,
            owner: "org".into(),
            repository: "project".into(),
            issue_number: 4,
            issue_id: 82,
            private: true,
            internal: false,
        },
    };
    IssueAdmission {
        authorization_id: IssueAdmission::id(&policy, "comment-21"),
        subscription: policy,
        event_id: "comment-21".into(),
        sender_id: 7,
        sender_automation: false,
        echo: false,
        workset: desk_diagnose_core::issue_authority::IssueWorkset {
            title: "macOS problem".into(),
            body: "Reproduce the macOS problem; Linux succeeds".into(),
            comments: vec![desk_diagnose_core::issue_authority::IssueCommentSnapshot {
                id: 1,
                author_id: 7,
                body: "Please investigate".into(),
            }],
        },
        issue,
        request_id: native_ids::stable(Kind::AssistantMainRequest, &"request-a"),
        execution_root_id: "native-root-a".into(),
        admitted_at_ms: 1000,
        expires_at_ms: Some(2000),
    }
}
fn reference(snapshot: &IssueAdmission) -> IssueSourceRef {
    IssueSourceRef {
        authorization_id: snapshot.authorization_id.clone(),
        subscription_revision: snapshot.subscription.revision,
        tool_ref: snapshot.issue.tool_ref(),
    }
}
fn origin(snapshot: &IssueAdmission) -> ReplyOrigin {
    ReplyOrigin {
        actor_id: snapshot.subscription.actor_id.clone(),
        root_conversation_id: snapshot.subscription.conversation_id.clone(),
        request_id: snapshot.request_id.clone(),
        execution_conversation_id: "worker-executor".into(),
        tool_call_id: "publish-1".into(),
        input_revision: 1,
        control_revision: 0,
        authorization_id: snapshot.authorization_id.clone(),
        authorization_revision: snapshot.subscription.revision,
    }
}

#[tokio::test]
async fn policy_revision_owner_and_repository_overlap_are_native() {
    let db = setup().await;
    let first = put(&db, policy(), 0, 1000).await.unwrap();
    assert_eq!(first.revision, 1);
    assert_eq!(first.allowed_sender_ids, vec![7, 12]);
    assert!(matches!(
        put(&db, policy(), 0, 1001).await,
        Err(AssistantStoreError::Stale {
            current_revision: 1
        })
    ));
    let mut other = first.clone();
    other.actor_id = "other".into();
    assert!(matches!(
        put(&db, other, 1, 1001).await,
        Err(AssistantStoreError::Inaccessible)
    ));
    let mut overlapping = first.clone();
    overlapping.subscription_id = native_ids::stable(Kind::IssueSubscription, &"other");
    overlapping.issue_filter = Some(IssueFilter {
        issue_id: 82,
        issue_number: 4,
    });
    assert!(put(&db, overlapping.clone(), 0, 1001).await.is_err());
    overlapping.repository.connection_ref = "second-credential".into();
    assert!(put(&db, overlapping.clone(), 0, 1001).await.is_err());
    overlapping.conversation_id = "other-principal".into();
    assert!(put(&db, overlapping, 0, 1001).await.is_ok());
    assert!(
        current(&db, "other", &first.subscription_id)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn permission_expansion_only_applies_to_later_admissions() {
    let db = setup().await;
    let mut initial = policy();
    initial.auto_reply = false;
    let first = put(&db, initial, 0, 1000).await.unwrap();
    let accepted = admitted(first.clone());
    admit(&db, &accepted).await.unwrap();
    let mut expanded = first;
    expanded.auto_reply = true;
    expanded.work_kind = IssueWorkKind::DeviceInvestigation;
    let expanded = put(&db, expanded, 1, 1100).await.unwrap();
    let (original, live) = resolve(&db, "owner", &reference(&accepted), &accepted.issue, 1200)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(live.revision, 2);
    assert!(!original.can_publish(&live, 1200));
    assert!(!original.can_use_devices(&live, 1200));
    assert_eq!(
        publication_revision(&db, &origin(&accepted), &accepted.issue, 1200)
            .await
            .unwrap(),
        None
    );
    let mut next = admitted(expanded);
    next.event_id = "comment-22".into();
    next.request_id = native_ids::stable(Kind::AssistantMainRequest, &"request-b");
    next.authorization_id = IssueAdmission::id(&next.subscription, &next.event_id);
    admit(&db, &next).await.unwrap();
    assert_eq!(
        publication_revision(&db, &origin(&next), &next.issue, 1200)
            .await
            .unwrap(),
        Some(2)
    );
    assert!(next.can_use_devices(&next.subscription, 1200));
}

#[tokio::test]
async fn revocation_sender_filter_pause_and_request_stop_are_read_from_current_policy() {
    let db = setup().await;
    let first = put(&db, policy(), 0, 1000).await.unwrap();
    let accepted = admitted(first.clone());
    admit(&db, &accepted).await.unwrap();
    let origin = origin(&accepted);
    assert_eq!(
        publication_revision(&db, &origin, &accepted.issue, 1100)
            .await
            .unwrap(),
        Some(1)
    );
    let mut narrowed = first;
    narrowed.auto_reply = false;
    let narrowed = put(&db, narrowed, 1, 1101).await.unwrap();
    assert!(
        resolve(&db, "owner", &reference(&accepted), &accepted.issue, 1200)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        publication_revision(&db, &origin, &accepted.issue, 1200)
            .await
            .unwrap(),
        None
    );
    let mut paused = narrowed;
    paused.enabled = false;
    let paused = put(&db, paused, 2, 1201).await.unwrap();
    assert!(
        resolve(&db, "owner", &reference(&accepted), &accepted.issue, 1202)
            .await
            .unwrap()
            .is_none()
    );
    let mut resumed = paused;
    resumed.enabled = true;
    resumed.auto_reply = true;
    let resumed = put(&db, resumed, 3, 1203).await.unwrap();
    assert_eq!(
        publication_revision(&db, &origin, &accepted.issue, 1204)
            .await
            .unwrap(),
        Some(4)
    );
    let mut sender_removed = resumed;
    sender_removed.allowed_sender_ids = vec![12];
    let sender_removed = put(&db, sender_removed, 4, 1205).await.unwrap();
    assert_eq!(
        publication_revision(&db, &origin, &accepted.issue, 1206)
            .await
            .unwrap(),
        None
    );
    let mut sender_restored = sender_removed;
    sender_restored.allowed_sender_ids.push(7);
    put(&db, sender_restored, 5, 1207).await.unwrap();
    stop_request(&db, "other", &accepted.request_id, 1208)
        .await
        .unwrap();
    assert_eq!(
        publication_revision(&db, &origin, &accepted.issue, 1209)
            .await
            .unwrap(),
        Some(6)
    );
    stop_request(&db, "owner", &accepted.request_id, 1210)
        .await
        .unwrap();
    assert_eq!(
        publication_revision(&db, &origin, &accepted.issue, 1211)
            .await
            .unwrap(),
        None
    );
    let replay = admit(&db, &accepted).await.unwrap();
    assert_eq!(replay.stopped_at_ms, Some(1210));
}

#[tokio::test]
async fn replay_never_renews_deadline_or_rebinds_a_request_and_remote_target() {
    let db = setup().await;
    let current = put(&db, policy(), 0, 1000).await.unwrap();
    let accepted = admitted(current);
    let original = admit(&db, &accepted).await.unwrap();
    assert_eq!(admit(&db, &accepted).await.unwrap(), original);
    for case in 0..4 {
        let mut changed = accepted.clone();
        match case {
            0 => changed.expires_at_ms = Some(3000),
            1 => changed.request_id = native_ids::stable(Kind::AssistantMainRequest, &"another"),
            2 => changed.issue.target.issue_id += 1,
            _ => changed.sender_id = 12,
        }
        assert!(admit(&db, &changed).await.is_err());
    }
    for case in 0..4 {
        let mut changed = origin(&accepted);
        match case {
            0 => changed.actor_id = "other".into(),
            1 => changed.root_conversation_id = "other-principal".into(),
            2 => changed.request_id = "other-request".into(),
            _ => changed.authorization_revision += 1,
        }
        assert_eq!(
            publication_revision(&db, &changed, &accepted.issue, 1100)
                .await
                .unwrap(),
            None
        );
    }
    assert_eq!(
        publication_revision(&db, &origin(&accepted), &accepted.issue, 2000)
            .await
            .unwrap(),
        None
    );
    let mut changed = accepted.issue.clone();
    changed.target.private = false;
    assert!(
        resolve(&db, "owner", &reference(&accepted), &changed, 1100)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn automation_echo_unknown_sender_and_inactive_rules_cannot_admit() {
    let db = setup().await;
    let current = put(&db, policy(), 0, 1000).await.unwrap();
    for case in 0..4 {
        let mut bad = admitted(current.clone());
        match case {
            0 => bad.sender_automation = true,
            1 => bad.echo = true,
            2 => bad.sender_id = 9,
            _ => bad.sender_id = 99,
        }
        assert!(admit(&db, &bad).await.is_err());
    }
    let mut disabled = current.clone();
    disabled.enabled = false;
    put(&db, disabled, 1, 1100).await.unwrap();
    assert!(admit(&db, &admitted(current)).await.is_err());
    assert_eq!(admission::Entity::find().all(&db).await.unwrap().len(), 0);
}

#[tokio::test]
async fn original_transaction_rolls_back_scope_update_and_stop_together() {
    let db = setup().await;
    let policy = put(&db, policy(), 0, 1000).await.unwrap();
    let accepted = admitted(policy.clone());
    admit(&db, &accepted).await.unwrap();
    let txn = db.begin().await.unwrap();
    let mut paused = policy.clone();
    paused.enabled = false;
    put(&txn, paused, 1, 1100).await.unwrap();
    stop_request(&txn, "owner", &accepted.request_id, 1100)
        .await
        .unwrap();
    txn.rollback().await.unwrap();
    assert_eq!(
        current(&db, "owner", &policy.subscription_id)
            .await
            .unwrap(),
        Some(policy)
    );
    assert_eq!(
        publication_revision(&db, &origin(&accepted), &accepted.issue, 1200)
            .await
            .unwrap(),
        Some(1)
    );
}

#[tokio::test]
async fn native_source_binding_checks_the_executor_and_stop_all_is_principal_scoped() {
    use desk_diagnose_core::{
        input_read_context::ReadContextSelection, session::PersistedAgentSession,
    };
    let db = setup().await;
    let policy = put(&db, policy(), 0, 1000).await.unwrap();
    let accepted = admitted(policy.clone());
    admit(&db, &accepted).await.unwrap();
    let mut original = ReadContextSelection::default();
    original.external_issues.push(accepted.issue.clone());
    original.external_issue_sources.push(reference(&accepted));
    let mut held = PersistedAgentSession::new(
        "native-root-a",
        "owner",
        "",
        1,
        desk_agent_protocol::AgentScope {
            granted: vec![],
            mode: desk_agent_protocol::ExecutionMode::ReadOnly,
            expires_at: None,
            policy_name: None,
        },
        "2026-11-05T00:00:00Z",
    );
    assert!(
        input_sources_current(&db, &held, &original, 1100)
            .await
            .unwrap()
    );
    held.conversation_id = "other-executor".into();
    assert!(
        !input_sources_current(&db, &held, &original, 1100)
            .await
            .unwrap()
    );
    held.conversation_id = accepted.execution_root_id.clone();
    let mut forged = original.clone();
    forged.external_issue_sources[0].subscription_revision += 1;
    assert!(
        !input_sources_current(&db, &held, &forged, 1100)
            .await
            .unwrap()
    );
    let mut second = policy;
    second.conversation_id = "another-principal".into();
    second.subscription_id = native_ids::stable(Kind::IssueSubscription, &"another-source");
    let second = put(&db, second, 0, 1000).await.unwrap();
    let other = admitted(second);
    admit(&db, &other).await.unwrap();
    let txn = db.begin().await.unwrap();
    stop_conversation(&txn, "owner", "principal", 1100)
        .await
        .unwrap();
    txn.rollback().await.unwrap();
    assert!(
        input_sources_current(&db, &held, &original, 1100)
            .await
            .unwrap()
    );
    stop_conversation(&db, "owner", "principal", 1100)
        .await
        .unwrap();
    assert!(
        !input_sources_current(&db, &held, &original, 1100)
            .await
            .unwrap()
    );
    assert_eq!(
        publication_revision(&db, &origin(&other), &other.issue, 1100)
            .await
            .unwrap(),
        Some(1)
    );
}

#[tokio::test]
async fn publication_preparation_uses_the_immutable_issue_snapshot_and_no_worker_history() {
    use desk_diagnose_core::{
        chat::{ChatMessage, ChatRole, ToolCall},
        input_read_context::ReadContextSelection,
        session::PersistedAgentSession,
    };
    let db = setup().await;
    let policy = put(&db, policy(), 0, 1000).await.unwrap();
    let accepted = admitted(policy);
    admit(&db, &accepted).await.unwrap();
    let mut held = PersistedAgentSession::new(
        "native-root-a",
        "owner",
        "",
        1,
        desk_agent_protocol::AgentScope {
            granted: vec![],
            mode: desk_agent_protocol::ExecutionMode::ReadOnly,
            expires_at: None,
            policy_name: None,
        },
        "2026-11-05T00:00:00Z",
    );
    held.input_revision = 1;
    held.conversation.push(ChatMessage::text(
        "private-history",
        ChatRole::Assistant,
        "PRIVATE_DEVICE_EVIDENCE",
    ));
    let mut original = ReadContextSelection::default();
    original.external_issues.push(accepted.issue.clone());
    original.external_issue_sources.push(reference(&accepted));
    let call = ToolCall {
        id: "publish-1".into(),
        name: desk_diagnose_core::issue_reply::PREPARE_TOOL_NAME.into(),
        arguments_json: serde_json::json!({"tool_ref":accepted.issue.tool_ref()}).to_string(),
    };
    let destination = desk_agent_protocol::data_lineage::DestinationIdentity::Model {
        connection_id: "gateway".into(),
        connection_revision: 1,
        model_id: "analysis".into(),
        profile_revision: 1,
    };
    let prepare = crate::issue_reply::preparation::prepare_on(
        &db,
        &held,
        &original,
        &call,
        &destination,
        1100,
    )
    .await
    .unwrap();
    let crate::issue_reply::preparation::Preparation::Generate(work) = &prepare else {
        panic!("Missing original snapshot");
    };
    assert!(work.material.text.contains("Linux succeeds"));
    assert!(!work.material.text.contains("PRIVATE_DEVICE_EVIDENCE"));
    let leaf = desk_diagnose_core::ai_assistant::source_registry::registered_leaf(
        work.material.data_envelope.as_ref().unwrap(),
        1,
        desk_diagnose_core::ai_assistant::source_registry::SOURCE_CONTRACT_REVISION,
    )
    .unwrap()
    .unwrap();
    assert_eq!(leaf.contract, desk_diagnose_core::ai_assistant::source_registry::RegisteredSourceContract::ExternalIssueResult);
    assert!(!leaf.policy.independent_fact_allowed);
    assert_eq!(work.origin.request_id, accepted.request_id);
    assert_eq!(work.origin.root_conversation_id, "principal");
    let mut changed = accepted.clone();
    changed.workset.body = "Edited after admission".into();
    assert!(admit(&db, &changed).await.is_err());
    assert_eq!(
        crate::issue_reply::preparation::prepare_on(
            &db,
            &held,
            &original,
            &call,
            &destination,
            1100
        )
        .await
        .unwrap(),
        prepare
    );
    original.external_issue_sources.clear();
    assert!(
        crate::issue_reply::preparation::prepare_on(
            &db,
            &held,
            &original,
            &call,
            &destination,
            1100
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn source_retirement_and_expiry_erase_snapshots_without_recreating_on_replay() {
    let db = setup().await;
    let policy = put(&db, policy(), 0, 1000).await.unwrap();
    let first = admitted(policy.clone());
    admit(&db, &first).await.unwrap();
    let mut second = admitted(policy);
    second.event_id = "comment-22".into();
    second.authorization_id = IssueAdmission::id(&second.subscription, &second.event_id);
    second.request_id = native_ids::stable(Kind::AssistantMainRequest, &"other-request");
    second.execution_root_id = "other-root".into();
    second.expires_at_ms = None;
    admit(&db, &second).await.unwrap();
    let txn = db.begin().await.unwrap();
    retire_conversation(&txn, "owner", "native-root-a", 1100)
        .await
        .unwrap();
    txn.rollback().await.unwrap();
    assert!(
        resolve(&db, "owner", &reference(&first), &first.issue, 1200)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(expire_on(&db, 2000, 1).await.unwrap(), 1);
    assert_eq!(expire_on(&db, 2000, 1).await.unwrap(), 0);
    let row = admission::Entity::find()
        .filter(admission::Column::AuthorizationId.eq(&first.authorization_id))
        .one(&db)
        .await
        .unwrap()
        .unwrap();
    assert!(row.snapshot_json.is_none());
    assert_eq!(row.retired_at_ms, Some(2000));
    assert!(admit(&db, &first).await.is_err());
    assert!(
        resolve(&db, "owner", &reference(&second), &second.issue, 2000)
            .await
            .unwrap()
            .is_some()
    );
    retire_conversation(&db, "other-actor", "principal", 2000)
        .await
        .unwrap();
    assert!(
        resolve(&db, "owner", &reference(&second), &second.issue, 2000)
            .await
            .unwrap()
            .is_some()
    );
    retire_conversation(&db, "owner", "principal", 2000)
        .await
        .unwrap();
    assert!(
        resolve(&db, "owner", &reference(&second), &second.issue, 2000)
            .await
            .unwrap()
            .is_none()
    );
    assert!(admit(&db, &second).await.is_err());
}
