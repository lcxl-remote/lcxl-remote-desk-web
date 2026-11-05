use super::*;
use desk_diagnose_core::{
    external_tools::{IssueAccess, IssuePlatform, IssueTarget},
    issue_authority::{IssueRepositoryScope, IssueWorkKind},
    native_ids::{self, Kind},
};
use sea_orm::{Database, DatabaseConnection, PaginatorTrait, Schema, TransactionTrait};

async fn setup() -> (DatabaseConnection, IssueSubscription, IssueSourceComment) {
    let db = Database::connect("sqlite::memory:").await.unwrap();
    crate::entity::register_schema(sea_orm::SchemaBuilder::new(Schema::new(
        db.get_database_backend(),
    )))
    .sync(&db)
    .await
    .unwrap();
    let repository = IssueRepositoryScope {
        platform: IssuePlatform::Gitea,
        connection_ref: "forge".into(),
        instance: "https://forge.example/".into(),
        account_id: 3,
        repository_id: 42,
        owner: "Org".into(),
        repository: "Project".into(),
        private: true,
        internal: false,
    };
    let policy = IssueSubscription {
        subscription_id: native_ids::stable(Kind::IssueSubscription, &"source"),
        revision: 0,
        actor_id: "owner".into(),
        conversation_id: "root".into(),
        enabled: true,
        repository: repository.clone(),
        issue_filter: None,
        allowed_sender_ids: vec![3, 7],
        automation_sender_ids: vec![8],
        work_kind: IssueWorkKind::Analysis,
        auto_reply: false,
    };
    let txn = db.begin().await.unwrap();
    let policy = crate::issue_authority::put(&txn, policy, 0, 1000)
        .await
        .unwrap();
    initialize_on(&txn, &policy, 1000).await.unwrap();
    txn.commit().await.unwrap();
    let comment = IssueSourceComment {
        repository,
        comment_id: 90,
        issue_number: Some(4),
        sender_id: Some(7),
        automation: false,
        created_at_ms: 1001,
        updated_at_ms: 1001,
        body: "First accepted body".into(),
    };
    (db, policy, comment)
}

async fn page(
    db: &DatabaseConnection,
    policy: &IssueSubscription,
    comments: &[IssueSourceComment],
    next: Option<u32>,
    now: i64,
) -> bool {
    let txn = db.begin().await.unwrap();
    let claim = claim_on(&txn, policy, &format!("lease-{now}"), now, now + 100)
        .await
        .unwrap()
        .unwrap();
    let finished = commit_page_on(
        &txn,
        &claim,
        comments,
        next.map(|p| p.try_into().unwrap()),
        now,
        now,
    )
    .await
    .unwrap();
    txn.commit().await.unwrap();
    finished
}

#[tokio::test]
async fn frozen_comments_survive_replays_edits_and_expanded_rules_without_readmission() {
    let (db, mut policy, original) = setup().await;
    let mut ignored = original.clone();
    ignored.comment_id += 1;
    ignored.sender_id = Some(9);
    assert!(
        !page(
            &db,
            &policy,
            &[original.clone(), ignored.clone()],
            Some(2),
            2000
        )
        .await
    );
    let first = event::Entity::find()
        .order_by_asc(event::Column::Id)
        .all(&db)
        .await
        .unwrap();
    assert_eq!(first.len(), 2);
    assert_eq!(first[0].state, "pending");
    assert_eq!(first[1].state, "ignored");
    assert!(first[1].snapshot_json.is_none());
    assert_eq!(first[1].sender_id, Some(9));
    let mut edited = original.clone();
    edited.body = "An edited command pretending to be owner instructions".into();
    edited.updated_at_ms += 100;
    let txn = db.begin().await.unwrap();
    policy.allowed_sender_ids.push(9);
    policy.auto_reply = true;
    policy.work_kind = IssueWorkKind::DeviceInvestigation;
    policy = crate::issue_authority::put(&txn, policy, 1, 2001)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    assert!(!page(&db, &policy, &[edited, ignored], None, 2002).await);
    let after = event::Entity::find()
        .order_by_asc(event::Column::Id)
        .all(&db)
        .await
        .unwrap();
    assert_eq!(after.len(), 2);
    assert_eq!(decode_comment(&after[0]).unwrap(), original);
    assert_ne!(after[0].snapshot_sha256, after[0].last_observation_sha256);
    assert_eq!(after[0].edited_at_ms, Some(2002));
    assert_eq!(after[1].state, "ignored");
    assert_eq!(after[1].reason, "sender_not_allowed");
    assert_eq!(after[1].subscription_revision, 1);
    let frozen_policy = original_policy(&after[0]).unwrap();
    assert_eq!(frozen_policy.revision, 1);
    assert!(!frozen_policy.auto_reply);
    assert_eq!(frozen_policy.work_kind, IssueWorkKind::Analysis);
    assert!(after[1].admitted_policy_json.is_none());
    assert!(
        after
            .iter()
            .all(|row| row.request_id.is_none() && row.consumed_at_ms.is_none())
    );
}

#[tokio::test]
async fn watermark_advances_only_after_two_complete_matching_passes() {
    let (db, policy, original) = setup().await;
    let mut other = original.clone();
    other.comment_id += 1;
    assert!(!page(&db, &policy, std::slice::from_ref(&original), Some(2), 2000).await);
    assert!(!page(&db, &policy, &[], None, 2001).await);
    // A deletion shifts offsets during the next pass. It must not complete the old window.
    assert!(!page(&db, &policy, std::slice::from_ref(&other), None, 2002).await);
    assert_eq!(
        source::Entity::find()
            .one(&db)
            .await
            .unwrap()
            .unwrap()
            .watermark_ms,
        1000
    );
    assert!(page(&db, &policy, std::slice::from_ref(&other), None, 2003).await);
    let completed = source::Entity::find().one(&db).await.unwrap().unwrap();
    assert_eq!(completed.watermark_ms, 2000);
    assert!(completed.scan_json.is_none());
    assert_eq!(event::Entity::find().count(&db).await.unwrap(), 2);
    assert_eq!(candidates_on(&db, 0, 2003, 10).await.unwrap().len(), 1);
    assert!(
        candidates_on(&db, completed.id, 2003, 10)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn lost_lease_and_paused_policy_reject_old_pages_before_recording_comments() {
    let (db, mut policy, comment) = setup().await;
    let txn = db.begin().await.unwrap();
    let old = claim_on(&txn, &policy, "old-node", 2000, 2010)
        .await
        .unwrap()
        .unwrap();
    txn.commit().await.unwrap();
    let txn = db.begin().await.unwrap();
    assert!(
        claim_on(&txn, &policy, "competitor", 2001, 2100)
            .await
            .unwrap()
            .is_none()
    );
    let current = claim_on(&txn, &policy, "new-node", 2011, 2100)
        .await
        .unwrap()
        .unwrap();
    txn.commit().await.unwrap();
    let txn = db.begin().await.unwrap();
    assert!(matches!(
        commit_page_on(&txn, &old, std::slice::from_ref(&comment), None, 2012, 2012).await,
        Err(AssistantStoreError::Inaccessible)
    ));
    txn.rollback().await.unwrap();
    assert_eq!(event::Entity::find().count(&db).await.unwrap(), 0);
    let txn = db.begin().await.unwrap();
    policy.enabled = false;
    policy = crate::issue_authority::put(&txn, policy, 1, 2012)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    let txn = db.begin().await.unwrap();
    assert!(matches!(
        commit_page_on(&txn, &current, &[comment], None, 2013, 2013).await,
        Err(AssistantStoreError::Inaccessible)
    ));
    assert!(
        claim_on(&txn, &policy, "paused", 2013, 2200)
            .await
            .unwrap()
            .is_none()
    );
    txn.rollback().await.unwrap();
    assert_eq!(event::Entity::find().count(&db).await.unwrap(), 0);
    let row = source::Entity::find().one(&db).await.unwrap().unwrap();
    assert_eq!(row.watermark_ms, 1000);
    assert_eq!(row.first_enabled_at_ms, 1000);
    let txn = db.begin().await.unwrap();
    policy.enabled = true;
    policy = crate::issue_authority::put(&txn, policy, 2, 2200)
        .await
        .unwrap();
    initialize_on(&txn, &policy, 2200).await.unwrap();
    let resumed = claim_on(&txn, &policy, "resumed", 2201, 2300)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resumed.row.first_enabled_at_ms, 1000);
    assert_eq!(resumed.scan.since_ms, 0);
    assert_eq!(resumed.scan.page.get(), 1);
    txn.rollback().await.unwrap();
}

#[tokio::test]
async fn failed_page_rolls_back_observations_and_retry_keeps_the_same_page() {
    let (db, policy, comment) = setup().await;
    let txn = db.begin().await.unwrap();
    let claim = claim_on(&txn, &policy, "reader", 2000, 2100)
        .await
        .unwrap()
        .unwrap();
    txn.commit().await.unwrap();
    let mut invalid = comment.clone();
    invalid.comment_id += 1;
    invalid.repository.repository_id += 1;
    let txn = db.begin().await.unwrap();
    assert!(
        commit_page_on(&txn, &claim, &[comment, invalid], None, 2001, 2001)
            .await
            .is_err()
    );
    txn.rollback().await.unwrap();
    assert_eq!(event::Entity::find().count(&db).await.unwrap(), 0);
    let txn = db.begin().await.unwrap();
    retry_on(&txn, &claim, 2002, 2050, "source_unavailable")
        .await
        .unwrap();
    txn.commit().await.unwrap();
    assert!(candidates_on(&db, 0, 2049, 10).await.unwrap().is_empty());
    let txn = db.begin().await.unwrap();
    let retry = claim_on(&txn, &policy, "reader-again", 2050, 2200)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(retry.scan, claim.scan);
    assert_eq!(retry.row.watermark_ms, 1000);
    txn.rollback().await.unwrap();
}

#[tokio::test]
async fn policy_updates_immediately_fence_old_readers_and_new_repository_activation_starts_a_new_window()
 {
    let (db, mut policy, comment) = setup().await;
    let txn = db.begin().await.unwrap();
    let old = claim_on(&txn, &policy, "old-policy-reader", 2000, 9000)
        .await
        .unwrap()
        .unwrap();
    txn.commit().await.unwrap();
    let txn = db.begin().await.unwrap();
    policy.allowed_sender_ids.push(9);
    policy = crate::issue_authority::put(&txn, policy, 1, 2001)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    let txn = db.begin().await.unwrap();
    assert!(matches!(
        commit_page_on(&txn, &old, &[comment], None, 2002, 2002).await,
        Err(AssistantStoreError::Inaccessible)
    ));
    let fresh = claim_on(&txn, &policy, "current-reader", 2002, 9100)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fresh.scan.started_at_ms, 2002);
    assert_eq!(fresh.row.watermark_ms, 1000);
    txn.rollback().await.unwrap();
    let txn = db.begin().await.unwrap();
    policy.enabled = false;
    policy = crate::issue_authority::put(&txn, policy, 2, 2003)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    assert_eq!(
        source::Entity::find()
            .one(&db)
            .await
            .unwrap()
            .unwrap()
            .first_enabled_at_ms,
        1000
    );
    let txn = db.begin().await.unwrap();
    policy.repository.repository_id += 1;
    policy = crate::issue_authority::put(&txn, policy, 3, 2004)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    assert_eq!(source::Entity::find().count(&db).await.unwrap(), 0);
    let txn = db.begin().await.unwrap();
    policy.enabled = true;
    crate::issue_authority::put(&txn, policy, 4, 3000)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    let row = source::Entity::find().one(&db).await.unwrap().unwrap();
    assert_eq!(row.first_enabled_at_ms, 3000);
    assert_eq!(row.watermark_ms, 3000);
}

#[tokio::test]
async fn explicit_stop_only_stops_accepted_events_and_removal_erases_bodies_and_inflight_ownership()
{
    let (db, policy, first) = setup().await;
    assert!(!page(&db, &policy, std::slice::from_ref(&first), Some(2), 2000).await);
    let txn = db.begin().await.unwrap();
    crate::issue_authority::stop_conversation(&txn, "owner", "root", 2001)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    let row = event::Entity::find().one(&db).await.unwrap().unwrap();
    assert_eq!(row.state, "stopped");
    assert_eq!(decode_comment(&row).unwrap(), first);
    let mut next = first.clone();
    next.comment_id += 1;
    assert!(!page(&db, &policy, std::slice::from_ref(&next), Some(3), 2002).await);
    let accepted = event::Entity::find()
        .order_by_asc(event::Column::Id)
        .all(&db)
        .await
        .unwrap();
    assert_eq!(accepted[0].state, "stopped");
    assert_eq!(accepted[1].state, "pending");
    let txn = db.begin().await.unwrap();
    let inflight = claim_on(&txn, &policy, "before-removal", 2003, 2100)
        .await
        .unwrap()
        .unwrap();
    txn.commit().await.unwrap();
    let txn = db.begin().await.unwrap();
    crate::issue_authority::retire_conversation(&txn, "owner", "root", 2004)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    let retired = event::Entity::find().all(&db).await.unwrap();
    assert!(retired.iter().all(|row| row.snapshot_json.is_none()
        && row.retired_at_ms == Some(2004)
        && row.state == "stopped"));
    assert_eq!(source::Entity::find().count(&db).await.unwrap(), 0);
    let txn = db.begin().await.unwrap();
    assert!(matches!(
        commit_page_on(&txn, &inflight, &[next], None, 2005, 2005).await,
        Err(AssistantStoreError::Inaccessible)
    ));
    txn.rollback().await.unwrap();
}

#[tokio::test]
async fn sqlite_reopen_restores_the_original_snapshot_cursor_and_first_activation() {
    let (_memory, policy, comment) = setup().await;
    let directory = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("source.sqlite").display()
    );
    let db = Database::connect(&url).await.unwrap();
    crate::entity::register_schema(sea_orm::SchemaBuilder::new(Schema::new(
        db.get_database_backend(),
    )))
    .sync(&db)
    .await
    .unwrap();
    let txn = db.begin().await.unwrap();
    let policy = crate::issue_authority::put(&txn, policy, 0, 1000)
        .await
        .unwrap();
    initialize_on(&txn, &policy, 1000).await.unwrap();
    txn.commit().await.unwrap();
    assert!(!page(&db, &policy, std::slice::from_ref(&comment), Some(2), 2000).await);
    db.close().await.unwrap();
    let reopened = Database::connect(&url).await.unwrap();
    let txn = reopened.begin().await.unwrap();
    let claim = claim_on(&txn, &policy, "after-restart", 2001, 2100)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claim.scan.page.get(), 2);
    assert_eq!(claim.row.watermark_ms, 1000);
    assert_eq!(claim.row.first_enabled_at_ms, 1000);
    let row = event::Entity::find().one(&txn).await.unwrap().unwrap();
    assert_eq!(decode_comment(&row).unwrap(), comment);
    txn.rollback().await.unwrap();
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn echoes_require_the_dispatched_issue_account_and_exact_body_or_native_comment_id() {
    let (db, policy, mut comment) = setup().await;
    comment.sender_id = Some(3);
    let marker = "1234567890abcdef1234567890abcdef";
    comment.body = format!("Frozen reply\n\n<!-- assistant-reply:{marker} -->");
    let key = desk_diagnose_core::issue_reply::IssueReplyReceiptKey {
        access: IssueAccess {
            connection_ref: policy.repository.connection_ref.clone(),
            target: IssueTarget {
                platform: policy.repository.platform,
                instance: policy.repository.instance.clone(),
                account_id: 3,
                repository_id: 42,
                owner: "Org".into(),
                repository: "Project".into(),
                issue_id: 82,
                issue_number: 4,
                private: true,
                internal: false,
            },
        },
        marker: marker.into(),
        wire_sha256: digest(&comment.body),
    };
    let json = serde_json::to_string(&key).unwrap();
    use crate::entity::agent_issue_reply as reply;
    reply::Entity::insert(reply::ActiveModel {
        artifact_id: Set("test-reply".into()),
        actor_id: Set("owner".into()),
        root_conversation_id: Set("root".into()),
        request_id: Set("request".into()),
        execution_conversation_id: Set("worker".into()),
        input_revision: Set(1),
        artifact_sha256: Set("".into()),
        receipt_sha256: Set(digest(&json)),
        receipt_json: Set(json),
        state: Set("unknown".into()),
        attempt: Set(1),
        dispatched_at_ms: Set(Some(2000)),
        reconciliation_page: Set(1),
        next_attempt_at_ms: Set(2000),
        created_at_ms: Set(2000),
        updated_at_ms: Set(2000),
        ..Default::default()
    })
    .exec_without_returning(&db)
    .await
    .unwrap();
    assert!(is_outgoing_echo_on(&db, "owner", &comment).await.unwrap());
    let mut other = comment.clone();
    other.sender_id = Some(7);
    assert!(!is_outgoing_echo_on(&db, "owner", &other).await.unwrap());
    other = comment.clone();
    other.issue_number = Some(5);
    assert!(!is_outgoing_echo_on(&db, "owner", &other).await.unwrap());
    other = comment.clone();
    other.body = other.body.replace("Frozen reply", "Altered reply");
    assert!(!is_outgoing_echo_on(&db, "owner", &other).await.unwrap());
    assert!(
        !is_outgoing_echo_on(&db, "another-owner", &comment)
            .await
            .unwrap()
    );
    assert!(!page(&db, &policy, std::slice::from_ref(&comment), None, 2001).await);
    let row = event::Entity::find().one(&db).await.unwrap().unwrap();
    assert_eq!(row.reason, "outgoing_echo");
    reply::Entity::update_many()
        .set(reply::ActiveModel {
            comment_id: Set(Some(90)),
            ..Default::default()
        })
        .exec(&db)
        .await
        .unwrap();
    comment.body = "The owner edited their original outgoing comment".into();
    assert!(is_outgoing_echo_on(&db, "owner", &comment).await.unwrap());
}

#[tokio::test]
async fn routed_work_uses_the_first_comment_and_header_without_expanding_original_authority() {
    use crate::{
        issue_authority,
        main_request::{self, FrozenRequestInput},
    };
    use desk_agent_protocol::{AgentScope, ExecutionMode, data_lineage::DestinationIdentity};
    use desk_diagnose_core::session::{AgentSessionSurface, PersistedAgentSession, TriggerOrigin};
    use routing::FrozenIssueHeader;

    let (db, mut policy, first) = setup().await;
    let mut root = PersistedAgentSession::new_workbench_coordinator(
        &policy.actor_id,
        &policy.conversation_id,
        None,
        desk_diagnose_core::assistant_policy::PERSONAL_ASSISTANT_POLICY_REVISION,
        AgentScope {
            granted: vec![],
            mode: ExecutionMode::ReadOnly,
            expires_at: None,
            policy_name: None,
        },
        "2026-11-05T00:00:00Z",
    )
    .unwrap();
    root.adopt_client_metadata(Some("client-root"), AgentSessionSurface::AiAssistant);
    let header = FrozenIssueHeader {
        issue: IssueAccess {
            connection_ref: policy.repository.connection_ref.clone(),
            target: IssueTarget {
                platform: policy.repository.platform,
                instance: policy.repository.instance.clone(),
                account_id: policy.repository.account_id,
                repository_id: policy.repository.repository_id,
                owner: policy.repository.owner.clone(),
                repository: policy.repository.repository.clone(),
                private: policy.repository.private,
                internal: policy.repository.internal,
                issue_number: first.issue_number.unwrap(),
                issue_id: 115,
            },
        },
        title: "The original issue title".into(),
        body: "The original issue body".into(),
    };
    let destination = DestinationIdentity::Model {
        connection_id: "gateway".into(),
        connection_revision: 1,
        model_id: "model".into(),
        profile_revision: 1,
    };
    page(&db, &policy, std::slice::from_ref(&first), None, 2000).await;
    let hint = routing::candidates_on(&db, 0, 4)
        .await
        .unwrap()
        .pop()
        .unwrap();
    let txn = db.begin().await.unwrap();
    let pending = routing::pending_on(&txn, &hint).await.unwrap().unwrap();
    assert!(
        routing::freeze_header_on(&txn, &pending, &header, 2001)
            .await
            .unwrap()
    );
    txn.commit().await.unwrap();

    let mut edited = first.clone();
    edited.body = "Edited: substitute a command for the work purpose".into();
    edited.updated_at_ms += 1;
    let txn = db.begin().await.unwrap();
    policy.work_kind = IssueWorkKind::DeviceInvestigation;
    policy.auto_reply = true;
    policy = issue_authority::put(&txn, policy, 1, 2002).await.unwrap();
    txn.commit().await.unwrap();
    page(&db, &policy, &[edited], None, 2003).await;

    let txn = db.begin().await.unwrap();
    let pending = routing::pending_on(&txn, &hint).await.unwrap().unwrap();
    assert!(!pending.can_use_devices());
    let mut edited_header = header.clone();
    edited_header.title = "A later issue title".into();
    assert!(
        !routing::freeze_header_on(&txn, &pending, &edited_header, 2004)
            .await
            .unwrap()
    );
    let request = routing::enqueue_on(&txn, &pending, &root, &[], &destination, 2004, 120_004)
        .await
        .unwrap();
    txn.rollback().await.unwrap();
    assert!(
        main_request::load(&db, &root.actor_id, &root.conversation_id, &request)
            .await
            .unwrap()
            .is_none()
    );
    assert!(routing::pending_on(&db, &hint).await.unwrap().is_some());

    let txn = db.begin().await.unwrap();
    let pending = routing::pending_on(&txn, &hint).await.unwrap().unwrap();
    assert_eq!(
        routing::enqueue_on(&txn, &pending, &root, &[], &destination, 2005, 120_005)
            .await
            .unwrap(),
        request
    );
    txn.commit().await.unwrap();
    assert!(routing::pending_on(&db, &hint).await.unwrap().is_none());
    let row = main_request::load(&db, &root.actor_id, &root.conversation_id, &request)
        .await
        .unwrap()
        .unwrap();
    let frozen = FrozenRequestInput::decode(&row).unwrap();
    assert!(frozen.targets.is_empty());
    assert!(!frozen.ask.question.contains(&first.body));
    let txn = db.begin().await.unwrap();
    let mut paused_policy = policy.clone();
    paused_policy.enabled = false;
    let paused = issue_authority::put(&txn, paused_policy, policy.revision, 2005)
        .await
        .unwrap();
    assert_eq!(
        frozen
            .issue_queue_authority(&txn, &root.actor_id, 2005)
            .await
            .unwrap(),
        main_request::IssueQueueAuthority::Paused
    );
    main_request::defer_paused_source(&txn, &row, 2005)
        .await
        .unwrap();
    let deferred = main_request::load(&txn, &root.actor_id, &root.conversation_id, &request)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(deferred.pending_input_json, row.pending_input_json);
    assert_eq!(deferred.coordination_attempts, row.coordination_attempts);
    assert_eq!(deferred.retry_at_ms, Some(7005));
    policy = issue_authority::put(&txn, policy, paused.revision, 2005)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    assert_eq!(
        frozen
            .issue_queue_authority(&db, &root.actor_id, 2005)
            .await
            .unwrap(),
        main_request::IssueQueueAuthority::Ready
    );
    let admitted = frozen
        .authorized_issue_on(&db, &root.actor_id, 2005)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(admitted.workset.title, header.title);
    assert_eq!(admitted.workset.comments[0].body, first.body);
    assert!(!admitted.can_use_devices(&policy, 2005));
    assert!(!admitted.can_publish(&policy, 2005));
    assert!(
        routing::has_open_work_on(&db, &root.actor_id, &root.conversation_id, &request)
            .await
            .unwrap()
    );
    let mut work = PersistedAgentSession::new_workbench_coordinator(
        &root.actor_id,
        &root.conversation_id,
        Some(&request),
        root.policy_revision,
        root.scope_snapshot.clone(),
        "2026-11-05T00:00:00Z",
    )
    .unwrap();
    work.bind_issue_work_source(&admitted).unwrap();
    assert!(work.validate_request_origin(TriggerOrigin::User).is_err());
    work.validate_request_origin(TriggerOrigin::IssueSubscription)
        .unwrap();
    assert!(
        issue_authority::authorized_session_on(&db, &work, false, 2005)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        issue_authority::authorized_session_on(&db, &work, true, 2005)
            .await
            .is_err()
    );
    work.adopt_trigger(TriggerOrigin::IssueSubscription, "first-turn");
    work.automation_turns_used = 3;
    work.review_denials.insert("execute".into(), 2);
    let native_before = work.clone();
    let mut followup = first.clone();
    followup.comment_id += 1;
    followup.body = "A frozen supplemental comment".into();
    let mut late = followup.clone();
    late.comment_id += 1;
    late.body = "A later unconsumed comment".into();
    page(&db, &policy, &[followup.clone(), late.clone()], None, 2006).await;
    let hints = routing::candidates_on(&db, 0, 4).await.unwrap();
    assert_eq!(hints.len(), 2);
    let txn = db.begin().await.unwrap();
    for hint in &hints {
        let pending = routing::pending_on(&txn, hint).await.unwrap().unwrap();
        assert!(
            routing::freeze_header_on(&txn, &pending, &header, 2007)
                .await
                .unwrap()
        );
        let pending = routing::pending_on(&txn, hint).await.unwrap().unwrap();
        let previous = routing::latest_round_on(&txn, &pending, 2007)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(previous.request_id, request);
        assert!(
            routing::attach_on(&txn, &pending, &previous, 2007)
                .await
                .unwrap()
        );
    }
    txn.commit().await.unwrap();
    let mut edited_followup = followup.clone();
    edited_followup.body = "Edit after attachment".into();
    edited_followup.updated_at_ms += 1;
    page(&db, &policy, &[edited_followup], None, 2008).await;
    let material = supplements::model_material_on(&db, &work, &destination, 2009)
        .await
        .unwrap();
    assert_eq!(material.len(), 2);
    assert_eq!(
        material[0].role,
        desk_diagnose_core::chat::ChatRole::UntrustedOutput
    );
    assert!(material[0].text.contains(&followup.body));
    assert!(!material[0].text.contains("Edit after attachment"));
    let labels = vec![material[0].data_envelope.clone().unwrap()];
    let txn = db.begin().await.unwrap();
    supplements::verify_model_material_on(&txn, &work, &destination, &labels, 2010, false)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    assert_eq!(routing::candidates_on(&db, 0, 4).await.unwrap().len(), 2);
    let txn = db.begin().await.unwrap();
    supplements::verify_model_material_on(&txn, &work, &destination, &labels, 2011, true)
        .await
        .unwrap();
    txn.rollback().await.unwrap();
    assert_eq!(routing::candidates_on(&db, 0, 4).await.unwrap().len(), 2);
    let txn = db.begin().await.unwrap();
    let mut wrong_label = labels[0].clone();
    wrong_label.digest_sha256 = "0".repeat(64);
    assert!(
        supplements::verify_model_material_on(
            &txn,
            &work,
            &destination,
            &[wrong_label],
            2012,
            true,
        )
        .await
        .is_err()
    );
    supplements::verify_model_material_on(&txn, &work, &destination, &labels, 2012, true)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    assert_eq!(work, native_before);
    let mut workset = admitted.clone();
    supplements::include_on(&db, &mut workset, &policy, true)
        .await
        .unwrap();
    assert_eq!(workset.workset.comments.len(), 2);
    assert_eq!(workset.workset.comments[1].body, followup.body);
    let remaining = routing::candidates_on(&db, 0, 4).await.unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].event_id, late.event_id());

    let worker = native_ids::stable(Kind::Worker, &"same-issue-worker");
    let first_source = work.agent_role.issue_source().unwrap();
    let txn = db.begin().await.unwrap();
    assert!(
        routing::preferred_worker_on(&txn, &root.actor_id, first_source)
            .await
            .unwrap()
            .is_none()
    );
    routing::record_first_worker_on(&txn, &root.actor_id, first_source, &worker)
        .await
        .unwrap();
    let pending = routing::pending_on(&txn, &remaining[0])
        .await
        .unwrap()
        .unwrap();
    assert!(
        routing::latest_round_on(&txn, &pending, 120_005)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        routing::release_attachment_on(&txn, &pending, 120_005)
            .await
            .unwrap()
    );
    let pending = routing::pending_on(&txn, &remaining[0])
        .await
        .unwrap()
        .unwrap();
    assert!(pending.header.is_none());
    let mut next_header = header.clone();
    next_header.title = "The issue changed before the next round".into();
    assert!(
        routing::freeze_header_on(&txn, &pending, &next_header, 120_005)
            .await
            .unwrap()
    );
    let pending = routing::pending_on(&txn, &remaining[0])
        .await
        .unwrap()
        .unwrap();
    let next_request =
        routing::enqueue_on(&txn, &pending, &root, &[], &destination, 120_005, 240_005)
            .await
            .unwrap();
    assert_ne!(request, next_request);
    let next_row = main_request::load(&txn, &root.actor_id, &root.conversation_id, &next_request)
        .await
        .unwrap()
        .unwrap();
    let next_input = FrozenRequestInput::decode(&next_row).unwrap();
    let next_admission = next_input
        .authorized_issue_on(&txn, &root.actor_id, 120_005)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(next_admission.workset.title, next_header.title);
    assert_eq!(
        next_admission.workset.comments.last().unwrap().body,
        late.body
    );
    let next_source = next_input.issue_source.as_deref().unwrap();
    assert_eq!(next_source.work_id, first_source.work_id);
    assert_eq!(
        routing::preferred_worker_on(&txn, &root.actor_id, next_source)
            .await
            .unwrap(),
        Some(worker),
    );
    txn.commit().await.unwrap();

    let mut stopped = late.clone();
    stopped.comment_id += 1;
    stopped.created_at_ms = 120_006;
    stopped.updated_at_ms = 120_006;
    page(&db, &policy, &[stopped], None, 120_007).await;
    let hint = routing::candidates_on(&db, 0, 4)
        .await
        .unwrap()
        .pop()
        .unwrap();
    let txn = db.begin().await.unwrap();
    let pending = routing::pending_on(&txn, &hint).await.unwrap().unwrap();
    routing::freeze_header_on(&txn, &pending, &header, 120_008)
        .await
        .unwrap();
    let pending = routing::pending_on(&txn, &hint).await.unwrap().unwrap();
    let next = routing::latest_round_on(&txn, &pending, 120_008)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(next.request_id, next_request);
    assert!(
        routing::attach_on(&txn, &pending, &next, 120_008)
            .await
            .unwrap()
    );
    issue_authority::stop_request(&txn, &root.actor_id, &next_request, 120_009)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    assert!(
        !next_input
            .issue_authority_current(&db, &root.actor_id, 120_009)
            .await
            .unwrap()
    );
    assert!(
        issue_authority::authorized_session_on(&db, &work, false, 120_009)
            .await
            .is_err()
    );
    assert!(routing::candidates_on(&db, 0, 4).await.unwrap().is_empty());

    let mut other_policy = policy.clone();
    other_policy.subscription_id = native_ids::stable(Kind::IssueSubscription, &"other-source");
    other_policy.repository.repository_id = 43;
    other_policy.repository.repository = "OtherProject".into();
    other_policy.allowed_sender_ids = vec![9];
    let txn = db.begin().await.unwrap();
    let other_policy = issue_authority::put(&txn, other_policy, 0, 120_010)
        .await.unwrap();
    txn.commit().await.unwrap();
    let mut other = first.clone();
    other.repository = other_policy.repository.clone();
    other.sender_id = Some(9);
    other.created_at_ms = 120_011;
    other.updated_at_ms = 120_011;
    page(&db, &other_policy, &[other], None, 120_012).await;
    let hint = routing::candidates_on(&db, 0, 4).await.unwrap().pop().unwrap();
    let txn = db.begin().await.unwrap();
    let pending = routing::pending_on(&txn, &hint).await.unwrap().unwrap();
    let mut other_header = header.clone();
    other_header.issue.target.repository_id = 43;
    other_header.issue.target.repository = "OtherProject".into();
    assert!(routing::freeze_header_on(&txn, &pending, &other_header, 120_013).await.unwrap());
    let pending = routing::pending_on(&txn, &hint).await.unwrap().unwrap();
    let other_request = routing::enqueue_on(&txn, &pending, &root, &[], &destination, 120_013, 240_013)
        .await.unwrap();
    let queued = main_request::load(&txn, &root.actor_id, &root.conversation_id, &other_request)
        .await.unwrap().unwrap();
    let other_frozen = FrozenRequestInput::decode(&queued).unwrap();
    let other_source = other_frozen.issue_source.as_deref().unwrap();
    assert_eq!(other_source.workbench_id, first_source.workbench_id);
    assert_ne!(other_source.work_id, first_source.work_id);
    assert_eq!(other_source.subscription_id, other_policy.subscription_id);
    assert!(issue_authority::resolve_work_source(
        &txn, &root.actor_id, other_source, &header.issue, 120_013,
    ).await.unwrap().is_none());
    txn.commit().await.unwrap();
}
