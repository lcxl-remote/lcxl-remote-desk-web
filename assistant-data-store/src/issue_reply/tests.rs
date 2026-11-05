use super::*;
use desk_agent_protocol::{
    AgentScope, ExecutionMode,
    data_lineage::{DestinationIdentity, Sensitivity},
};
use desk_diagnose_core::{
    chat::{ChatMessage, ChatRole, ModelTurn, ToolCall},
    external_tools::{IssueAccess, IssueTarget, label_issue_read},
    input_read_context::ReadContextSelection,
    issue_reply::ReplyOrigin,
    model_egress::{
        ModelEgressPolicy, NativeModelOutputEvidence, NativeModelOutputSources,
        project_model_input_lineage,
    },
    session::PersistedAgentSession,
};
use sea_orm::{Database, Schema, SchemaBuilder};

fn artifact(mixed: bool) -> FrozenIssueReply {
    artifact_for(mixed, "request-a")
}

fn artifact_for(mixed: bool, request_id: &str) -> FrozenIssueReply {
    artifact_for_fence(mixed, request_id, 0, 1)
}

fn artifact_for_fence(
    mixed: bool,
    request_id: &str,
    control_revision: u64,
    authorization_revision: u64,
) -> FrozenIssueReply {
    artifact_with_expiry(
        mixed,
        request_id,
        control_revision,
        authorization_revision,
        None,
    )
}

fn artifact_with_expiry(
    mixed: bool,
    request_id: &str,
    control_revision: u64,
    authorization_revision: u64,
    expires_at: Option<u64>,
) -> FrozenIssueReply {
    let origin = ReplyOrigin {
        actor_id: "owner".into(),
        root_conversation_id: "root".into(),
        request_id: request_id.into(),
        execution_conversation_id: format!("work-{request_id}"),
        tool_call_id: "publish-1".into(),
        input_revision: 1,
        control_revision,
        authorization_id: "subscription-grant".into(),
        authorization_revision,
    };
    artifact_for_origin(mixed, origin, expires_at)
}

fn artifact_for_origin(
    mixed: bool,
    origin: ReplyOrigin,
    expires_at: Option<u64>,
) -> FrozenIssueReply {
    let scope = AgentScope {
        granted: vec![],
        mode: ExecutionMode::ReadOnly,
        expires_at: None,
        policy_name: None,
    };
    let conversation = origin.execution_conversation_id.clone();
    let mut session =
        PersistedAgentSession::new(&conversation, "owner", "", 1, scope.clone(), "now");
    session.input_revision = origin.input_revision;
    session.control_revision = origin.control_revision;
    let access = IssueAccess {
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
    let mut original = ReadContextSelection::capture(&[], &scope).unwrap();
    original.external_issues.push(access.clone());
    let destination = DestinationIdentity::Model {
        connection_id: "gateway".into(),
        connection_revision: 1,
        model_id: "model".into(),
        profile_revision: 1,
    };
    let call = ToolCall {
        id: "read-issue".into(),
        name: "call_external_tool".into(),
        arguments_json: serde_json::json!({"tool_ref":access.tool_ref(),"args":{"method":"get"}})
            .to_string(),
    };
    let content = r#"{"title":"macOS problem"}"#;
    let mut label = label_issue_read(
        &session,
        &original,
        &destination,
        &call,
        &"a".repeat(64),
        content,
    )
    .unwrap();
    label.retention.expires_at_unix_ms = expires_at;
    let mut material = ChatMessage::text("issue-material", ChatRole::UntrustedOutput, content);
    material.data_envelope = Some(label);

    let policy = ModelEgressPolicy {
        destination: destination.clone(),
        selected_source_tools: Default::default(),
        export_authorization_id: desk_diagnose_core::issue_reply::publication_export_id(
            &origin, &access,
        ),
        now_unix_ms: 1000,
        byte_cap: 1024 * 1024,
        permission_resume: false,
    };
    let prepared = policy
        .authorize_issue_reply_request(&access.target.destination("forge".into()), vec![material])
        .unwrap();
    let prepared = if mixed {
        let mut request = prepared.request;
        let mut private = ChatMessage::text(
            "private-evidence",
            ChatRole::UntrustedOutput,
            "Private evidence",
        );
        let mut label = request.messages[1].data_envelope.clone().unwrap();
        label.envelope_id = "private-evidence".into();
        label.provenance.source_provider_id = "fixture-device".into();
        label.provenance.source_tool_name = "read_recent_logs".into();
        label.provenance.source_object_id = None;
        label.digest_sha256 = digest(&private.text);
        label.content = desk_agent_protocol::data_lineage::ContentRef::ImmutableBlob {
            blob_id: "private-evidence".into(),
            sha256: label.digest_sha256.clone(),
            size_bytes: private.text.len() as u64,
            media_type: "text/plain".into(),
        };
        label.allowed_destinations = vec![destination.clone()];
        label.sensitivity = Sensitivity::Sensitive;
        private.data_envelope = Some(label);
        request.messages.push(private);
        policy.authorize_request(request).unwrap()
    } else {
        prepared
    };
    let turn = ModelTurn {
        text: "The issue is under investigation.".into(),
        ..Default::default()
    };
    let output = policy
        .derive_model_output_envelope(&turn, &prepared.input_envelopes)
        .unwrap();
    let inputs = project_model_input_lineage(&prepared.audit, &prepared.input_envelopes).unwrap();
    let snapshot = prepared.input_snapshot().unwrap();
    let evidence = NativeModelOutputEvidence::from_receipt(
        "physical-call",
        &conversation,
        &policy.export_authorization_id,
        1000,
        1001,
        &prepared.audit,
        &inputs,
        &snapshot,
        &output,
    )
    .unwrap();
    FrozenIssueReply::freeze(
        origin,
        access,
        turn.text,
        output,
        NativeModelOutputSources {
            evidence,
            inputs,
            input_snapshot: snapshot,
        },
        "b".repeat(64),
        &[3; 16],
    )
    .unwrap()
}

async fn setup() -> sea_orm::DatabaseConnection {
    let db = Database::connect("sqlite::memory:").await.unwrap();
    crate::entity::register_schema(SchemaBuilder::new(Schema::new(db.get_database_backend())))
        .sync(&db)
        .await
        .unwrap();
    db
}

fn receipt(artifact: &FrozenIssueReply, id: u64) -> IssueReplyReceipt {
    IssueReplyReceipt {
        comment_id: id,
        account_id: artifact.access.target.account_id,
        body: artifact.wire_body(),
    }
}

#[tokio::test]
async fn first_freeze_owns_the_marker_and_unpublishable_sources_stay_internal() {
    let db = setup().await;
    let original = artifact(false);
    let row = freeze(&db, &original, DeliveryMode::Automatic, 1002)
        .await
        .unwrap();
    assert_eq!(row.state, READY);
    let mut duplicate = original.clone();
    duplicate.marker = "c".repeat(32);
    let replay = freeze(&db, &duplicate, DeliveryMode::Automatic, 1003)
        .await
        .unwrap();
    assert_eq!(row, replay);
    assert_eq!(decode(&replay).unwrap().marker, original.marker);
    let mut corrupt = row.clone();
    corrupt.artifact_json.as_mut().unwrap().push(' ');
    assert!(decode(&corrupt).is_err());
    let mut altered = original.clone();
    altered.body.push_str("Changed");
    assert!(
        freeze(&db, &altered, DeliveryMode::Automatic, 1004)
            .await
            .is_err()
    );
    let conflict = artifact(true);
    assert_eq!(conflict.artifact_id, original.artifact_id);
    assert!(
        freeze(&db, &conflict, DeliveryMode::Automatic, 1002)
            .await
            .is_err()
    );
    let mixed = artifact_for(true, "request-mixed");
    assert!(mixed.authorize_delivery(1002).is_err());
    let blocked = freeze(&db, &mixed, DeliveryMode::Automatic, 1002)
        .await
        .unwrap();
    assert_eq!(blocked.state, INTERNAL);
    assert_eq!(blocked.reason.as_deref(), Some("source_not_publishable"));
    assert_eq!(decode(&blocked).unwrap().body, mixed.body);
    let internal = artifact_for(false, "request-internal");
    let internal = freeze(&db, &internal, DeliveryMode::Internal, 1002)
        .await
        .unwrap();
    assert_eq!(internal.state, INTERNAL);
}

#[tokio::test]
async fn dispatch_recovery_only_reconciles_and_late_exact_receipts_settle_the_original_action() {
    let db = setup().await;
    let artifact = artifact(false);
    freeze(&db, &artifact, DeliveryMode::Automatic, 1002)
        .await
        .unwrap();
    let first = claim_next(&db, "node-a", "claim-a", 1002, 2002)
        .await
        .unwrap()
        .unwrap();
    assert!(!first.needs_reconciliation());
    assert!(
        claim_next(&db, "node-b", "claim-b", 1002, 2002)
            .await
            .unwrap()
            .is_none()
    );
    assert!(mark_dispatched(&db, &first, 0, 1003).await.is_err());
    assert!(
        mark_dispatched(&db, &first, i64::MAX as u64 + 1, 1003)
            .await
            .is_err()
    );
    assert!(mark_dispatched(&db, &first, 3, 1003).await.unwrap());
    assert!(!release_unsent(&db, &first, 1004, 1005).await.unwrap());
    recover(&db, 2002).await.unwrap();
    let second = claim_next(&db, "node-b", "claim-b", 2002, 3002)
        .await
        .unwrap()
        .unwrap();
    assert!(second.needs_reconciliation());
    assert_eq!(second.row.dispatch_token.as_deref(), Some("claim-a"));
    assert!(!mark_dispatched(&db, &second, 1, 2003).await.unwrap());
    assert!(!release_unsent(&db, &first, 2003, 2004).await.unwrap());
    let mut wrong = receipt(&artifact, 9);
    wrong.account_id += 1;
    assert!(
        record_receipt(&db, &second, "claim-a", &wrong, 2003)
            .await
            .is_err()
    );
    wrong = receipt(&artifact, 9);
    wrong.body.push_str("Modified");
    assert!(
        record_receipt(&db, &second, "claim-a", &wrong, 2003)
            .await
            .is_err()
    );
    assert!(
        !record_receipt(&db, &second, "claim-b", &receipt(&artifact, 9), 2003)
            .await
            .unwrap()
    );
    assert!(
        defer_reconciliation(
            &db,
            &second,
            2003,
            2503,
            std::num::NonZeroU32::new(2).unwrap()
        )
        .await
        .unwrap()
    );
    assert_eq!(
        load(&db, "owner", &artifact.artifact_id)
            .await
            .unwrap()
            .unwrap()
            .reconciliation_page,
        2
    );
    assert!(
        claim_next(&db, "node-c", "claim-c", 2004, 3004)
            .await
            .unwrap()
            .is_none()
    );
    // The original sender may report success after a new reconciler released its claim.
    assert!(
        record_receipt(&db, &first, "claim-a", &receipt(&artifact, 9), 2005)
            .await
            .unwrap()
    );
    let saved = load(&db, "owner", &artifact.artifact_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved.state, DELIVERED);
    assert_eq!(saved.comment_id, Some(9));
    assert_eq!(saved.dispatched_authorization_revision, Some(3));
    assert_eq!(saved.request_id, "request-a");
    assert!(
        !record_receipt(&db, &second, "claim-a", &receipt(&artifact, 10), 2006)
            .await
            .unwrap()
    );
    assert!(
        !cancel(&db, "owner", &artifact.artifact_id, 2006)
            .await
            .unwrap()
    );
    assert!(
        claim_next(&db, "node-d", "claim-d", 5000, 6000)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn cancellation_distinguishes_unsent_from_unknown_and_does_not_consume_other_work() {
    let db = setup().await;
    let artifact = artifact(false);
    freeze(&db, &artifact, DeliveryMode::Automatic, 1002)
        .await
        .unwrap();
    let first = claim_next(&db, "node-a", "claim-a", 1002, 2002)
        .await
        .unwrap()
        .unwrap();
    assert!(
        !cancel(&db, "another-owner", &artifact.artifact_id, 1003)
            .await
            .unwrap()
    );
    assert!(
        cancel(&db, "owner", &artifact.artifact_id, 1003)
            .await
            .unwrap()
    );
    assert!(!mark_dispatched(&db, &first, 1, 1004).await.unwrap());
    assert_eq!(
        load(&db, "owner", &artifact.artifact_id)
            .await
            .unwrap()
            .unwrap()
            .state,
        NOT_SENT
    );
    let second = artifact_for(false, "request-b");
    freeze(&db, &second, DeliveryMode::Automatic, 1004)
        .await
        .unwrap();
    let second_claim = claim_next(&db, "node-a", "claim-b", 1004, 2004)
        .await
        .unwrap()
        .unwrap();
    assert!(mark_dispatched(&db, &second_claim, 1, 1005).await.unwrap());
    assert!(
        cancel(&db, "owner", &second.artifact_id, 1006)
            .await
            .unwrap()
    );
    assert_eq!(
        load(&db, "owner", &second.artifact_id)
            .await
            .unwrap()
            .unwrap()
            .state,
        UNKNOWN
    );
    assert!(
        record_receipt(&db, &second_claim, "claim-b", &receipt(&second, 8), 1007)
            .await
            .unwrap()
    );
    assert_eq!(
        load(&db, "owner", &artifact.artifact_id)
            .await
            .unwrap()
            .unwrap()
            .state,
        NOT_SENT
    );
}

#[tokio::test]
async fn two_connections_share_claims_and_restart_keeps_the_dispatch_generation() {
    let directory = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite:{}?mode=rwc",
        directory.path().join("reply.db").display()
    );
    let first = Database::connect(&url).await.unwrap();
    crate::entity::register_schema(SchemaBuilder::new(Schema::new(
        first.get_database_backend(),
    )))
    .sync(&first)
    .await
    .unwrap();
    let second = Database::connect(&url).await.unwrap();
    let artifact = artifact(false);
    let mut contender = artifact.clone();
    contender.marker = "f".repeat(32);
    let (a, b) = tokio::join!(
        freeze(&first, &artifact, DeliveryMode::Automatic, 1002),
        freeze(&second, &contender, DeliveryMode::Automatic, 1002)
    );
    assert_eq!(a.unwrap(), b.unwrap());
    let (a, b) = tokio::join!(
        claim_next(&first, "node-a", "claim-a", 1002, 2002),
        claim_next(&second, "node-b", "claim-b", 1002, 2002)
    );
    let a = a.unwrap();
    let b = b.unwrap();
    assert_ne!(a.is_some(), b.is_some());
    let claim = a.or(b).unwrap();
    assert!(mark_dispatched(&first, &claim, 4, 1003).await.unwrap());
    first.close().await.unwrap();
    second.close().await.unwrap();
    let reopened = Database::connect(&url).await.unwrap();
    let recovered = claim_next(&reopened, "node-c", "claim-c", 2002, 3002)
        .await
        .unwrap()
        .unwrap();
    assert!(recovered.needs_reconciliation());
    assert_eq!(recovered.row.dispatched_authorization_revision, Some(4));
    assert_eq!(recovered.row.dispatch_token, claim.row.claim_token);
    assert_eq!(recovered.artifact, claim.artifact);
    assert!(
        record_receipt(
            &reopened,
            &recovered,
            claim.row.claim_token.as_deref().unwrap(),
            &receipt(claim.publication().unwrap(), 9),
            2003
        )
        .await
        .unwrap()
    );
}

#[tokio::test]
async fn an_unsent_expired_claim_can_be_reclaimed_and_released_with_backoff() {
    let db = setup().await;
    let artifact = artifact(false);
    freeze(&db, &artifact, DeliveryMode::Automatic, 1002)
        .await
        .unwrap();
    let first = claim_next(&db, "node-a", "claim-a", 1002, 1004)
        .await
        .unwrap()
        .unwrap();
    let second = claim_next(&db, "node-b", "claim-b", 1004, 2004)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(second.row.attempt, 2);
    assert!(!second.needs_reconciliation());
    assert!(second.row.dispatch_token.is_none());
    assert!(!mark_dispatched(&db, &first, 1, 1005).await.unwrap());
    assert!(release_unsent(&db, &second, 1005, 1010).await.unwrap());
    assert!(
        claim_next(&db, "node-c", "claim-c", 1006, 2006)
            .await
            .unwrap()
            .is_none()
    );
    let third = claim_next(&db, "node-c", "claim-c", 1010, 2010)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(third.row.attempt, 3);
    assert!(mark_dispatched(&db, &third, 1, 1011).await.unwrap());
    assert!(
        !mark_unknown(&db, &artifact.artifact_id, "claim-b", 1012)
            .await
            .unwrap()
    );
    assert!(
        mark_unknown(&db, &artifact.artifact_id, "claim-c", 1012)
            .await
            .unwrap()
    );
    let fourth = claim_next(&db, "node-d", "claim-d", 1013, 2013)
        .await
        .unwrap()
        .unwrap();
    assert!(fourth.needs_reconciliation());
    assert_eq!(fourth.row.dispatch_token.as_deref(), Some("claim-c"));
}

#[tokio::test]
async fn a_corrupt_artifact_does_not_block_other_work_or_rearm_unknown_effects() {
    let db = setup().await;
    let a = artifact(false);
    let b = artifact_for(false, "request-b");
    freeze(&db, &a, DeliveryMode::Automatic, 1002)
        .await
        .unwrap();
    let first = claim_next(&db, "node-a", "claim-a", 1002, 2002)
        .await
        .unwrap()
        .unwrap();
    mark_dispatched(&db, &first, 1, 1003).await.unwrap();
    mark_unknown(&db, &a.artifact_id, "claim-a", 1004)
        .await
        .unwrap();
    reply::Entity::update_many()
        .filter(reply::Column::ArtifactId.eq(&a.artifact_id))
        .col_expr(reply::Column::ArtifactJson, Expr::value("invalid-json"))
        .exec(&db)
        .await
        .unwrap();
    freeze(&db, &b, DeliveryMode::Automatic, 1005)
        .await
        .unwrap();
    let next = claim_next(&db, "node-b", "claim-b", 1005, 2005)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(next.row.artifact_id, b.artifact_id);
    let invalid = load(&db, "owner", &a.artifact_id).await.unwrap().unwrap();
    assert_eq!(invalid.state, UNKNOWN);
    assert_eq!(invalid.dispatch_token.as_deref(), Some("claim-a"));
    assert_eq!(invalid.reason.as_deref(), Some("artifact_invalid"));
    assert!(
        claim_next(&db, "node-c", "claim-c", 1005, 2005)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn changing_a_control_or_grant_revision_cannot_republish_the_same_native_call() {
    let db = setup().await;
    let first = artifact(false);
    let saved = freeze(&db, &first, DeliveryMode::Automatic, 1002)
        .await
        .unwrap();
    for changed in [
        artifact_for_fence(false, "request-a", 3, 1),
        artifact_for_fence(false, "request-a", 0, 2),
    ] {
        changed.validate().unwrap();
        assert_eq!(changed.artifact_id, first.artifact_id);
        assert!(
            freeze(&db, &changed, DeliveryMode::Automatic, 1003)
                .await
                .is_err()
        );
    }
    assert_eq!(
        load(&db, "owner", &first.artifact_id)
            .await
            .unwrap()
            .unwrap(),
        saved
    );
}

#[tokio::test]
async fn stopping_one_executor_keeps_results_and_leaves_other_workers_running() {
    use sea_orm::TransactionTrait;
    let db = setup().await;
    let a = artifact(false);
    let b = artifact_for(false, "request-b");
    freeze(&db, &a, DeliveryMode::Automatic, 1002)
        .await
        .unwrap();
    freeze(&db, &b, DeliveryMode::Automatic, 1002)
        .await
        .unwrap();
    let txn = db.begin().await.unwrap();
    lifecycle::cancel_request_on(
        &txn,
        "other-owner",
        &a.origin.execution_conversation_id,
        a.origin.input_revision,
        "work_stopped",
        1003,
    )
    .await
    .unwrap();
    lifecycle::cancel_request_on(
        &txn,
        "owner",
        &a.origin.execution_conversation_id,
        a.origin.input_revision,
        "work_stopped",
        1003,
    )
    .await
    .unwrap();
    txn.commit().await.unwrap();
    let stopped = load(&db, "owner", &a.artifact_id).await.unwrap().unwrap();
    assert_eq!(stopped.state, NOT_SENT);
    assert_eq!(decode(&stopped).unwrap(), a);
    let running = load(&db, "owner", &b.artifact_id).await.unwrap().unwrap();
    assert_eq!(running.state, READY);
    assert_eq!(decode(&running).unwrap(), b);
}

#[tokio::test]
async fn removal_erases_result_data_but_retains_a_receipt_key_for_unknown_effects() {
    use sea_orm::TransactionTrait;
    let db = setup().await;
    let a = artifact(false);
    freeze(&db, &a, DeliveryMode::Automatic, 1002)
        .await
        .unwrap();
    let first = claim_next(&db, "node-a", "claim-a", 1002, 2002)
        .await
        .unwrap()
        .unwrap();
    mark_dispatched(&db, &first, 2, 1003).await.unwrap();
    let txn = db.begin().await.unwrap();
    lifecycle::retire_conversation_on(&txn, "owner", "root", 1004)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    let erased = load(&db, "owner", &a.artifact_id).await.unwrap().unwrap();
    assert_eq!(erased.state, UNKNOWN);
    assert_eq!(erased.retired_at_ms, Some(1004));
    assert!(erased.artifact_json.is_none());
    assert!(decode(&erased).is_err());
    assert!(!erased.receipt_json.contains(&a.body));
    assert!(!erased.receipt_json.contains("macOS problem"));
    assert!(
        receipt_key(&erased)
            .unwrap()
            .matches_receipt(3, &a.wire_body())
    );
    let reconciler = claim_next(&db, "node-b", "claim-b", 1005, 2005)
        .await
        .unwrap()
        .unwrap();
    assert!(reconciler.artifact.is_none());
    assert!(reconciler.needs_reconciliation());
    assert!(mark_dispatched(&db, &reconciler, 3, 1006).await.is_err());
    // Repeated cleanup must not take a live reconciliation claim away.
    let txn = db.begin().await.unwrap();
    lifecycle::retire_conversation_on(&txn, "owner", "root", 1006)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    assert_eq!(
        load(&db, "owner", &a.artifact_id).await.unwrap().unwrap(),
        reconciler.row
    );
    assert!(
        record_receipt(&db, &reconciler, "claim-a", &receipt(&a, 9), 1007)
            .await
            .unwrap()
    );
    let done = load(&db, "owner", &a.artifact_id).await.unwrap().unwrap();
    assert_eq!(done.state, DELIVERED);
    assert_eq!(done.comment_id, Some(9));
    assert_eq!(done.dispatched_authorization_revision, Some(2));
    assert!(done.artifact_json.is_none());
}

#[tokio::test]
async fn replay_after_removal_never_restores_a_body_or_rearms_delivery() {
    use sea_orm::TransactionTrait;
    let db = setup().await;
    let a = artifact(false);
    freeze(&db, &a, DeliveryMode::Automatic, 1002)
        .await
        .unwrap();
    let txn = db.begin().await.unwrap();
    lifecycle::retire_conversation_on(&txn, "owner", "root", 1003)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    let erased = load(&db, "owner", &a.artifact_id).await.unwrap().unwrap();
    assert_eq!(erased.state, NOT_SENT);
    let mut retry = a.clone();
    retry.marker = "e".repeat(32);
    assert_eq!(
        freeze(&db, &retry, DeliveryMode::Automatic, 1004)
            .await
            .unwrap(),
        erased
    );
    assert!(
        claim_next(&db, "node-a", "claim-a", 1004, 2004)
            .await
            .unwrap()
            .is_none()
    );
    let changed = artifact_for_fence(false, "request-a", 1, 1);
    assert!(
        freeze(&db, &changed, DeliveryMode::Automatic, 1004)
            .await
            .is_err()
    );
    assert!(
        load(&db, "owner", &a.artifact_id)
            .await
            .unwrap()
            .unwrap()
            .artifact_json
            .is_none()
    );
}

#[tokio::test]
async fn result_removal_rolls_back_with_the_original_native_transaction() {
    use sea_orm::TransactionTrait;
    let db = setup().await;
    let a = artifact(false);
    let original = freeze(&db, &a, DeliveryMode::Automatic, 1002)
        .await
        .unwrap();
    let txn = db.begin().await.unwrap();
    lifecycle::retire_conversation_on(&txn, "owner", "root", 1003)
        .await
        .unwrap();
    assert!(
        load(&txn, "owner", &a.artifact_id)
            .await
            .unwrap()
            .unwrap()
            .artifact_json
            .is_none()
    );
    txn.rollback().await.unwrap();
    assert_eq!(
        load(&db, "owner", &a.artifact_id).await.unwrap().unwrap(),
        original
    );
}

#[tokio::test]
async fn source_expiry_erases_all_result_states_in_bounded_transactional_batches() {
    let db = setup().await;
    let first = artifact_with_expiry(false, "expiry-a", 0, 1, Some(2000));
    let second = artifact_with_expiry(false, "expiry-b", 0, 1, Some(2000));
    let future = artifact_with_expiry(false, "future", 0, 1, Some(3000));
    let a = freeze(&db, &first, DeliveryMode::Internal, 1000)
        .await
        .unwrap();
    let b = freeze(&db, &second, DeliveryMode::Automatic, 1000)
        .await
        .unwrap();
    let c = freeze(&db, &future, DeliveryMode::Automatic, 1000)
        .await
        .unwrap();
    assert_eq!(a.content_expires_at_ms, Some(2000));
    assert_eq!(b.content_expires_at_ms, Some(2000));
    assert_eq!(lifecycle::expire_on(&db, 1999, 10).await.unwrap(), 0);
    use sea_orm::TransactionTrait;
    let txn = db.begin().await.unwrap();
    assert_eq!(lifecycle::expire_on(&txn, 2000, 1).await.unwrap(), 1);
    txn.rollback().await.unwrap();
    assert_eq!(
        load(&db, "owner", &a.artifact_id).await.unwrap().unwrap(),
        a
    );
    assert_eq!(lifecycle::expire_on(&db, 2000, 1).await.unwrap(), 1);
    let erased = load(&db, "owner", &a.artifact_id).await.unwrap().unwrap();
    assert!(erased.artifact_json.is_none());
    assert_eq!(erased.reason.as_deref(), Some("source_expired"));
    assert_eq!(
        load(&db, "owner", &b.artifact_id).await.unwrap().unwrap(),
        b
    );
    assert_eq!(lifecycle::expire_on(&db, 2000, 1).await.unwrap(), 1);
    assert_eq!(lifecycle::expire_on(&db, 2000, 10).await.unwrap(), 0);
    assert_eq!(
        load(&db, "owner", &c.artifact_id).await.unwrap().unwrap(),
        c
    );
}

#[tokio::test]
async fn stopping_b_keeps_a_delivery_in_the_same_conversation_and_original_generation() {
    let db = setup().await;
    let mut origin = artifact(false).origin;
    origin.execution_conversation_id = "continuous-conversation".into();
    let a = artifact_for_origin(false, origin.clone(), None);
    origin.request_id = "request-b".into();
    origin.input_revision = 2;
    let b = artifact_for_origin(false, origin, None);
    freeze(&db, &a, DeliveryMode::Automatic, 1000)
        .await
        .unwrap();
    freeze(&db, &b, DeliveryMode::Automatic, 1001)
        .await
        .unwrap();
    let claim = claim_next(&db, "node-a", "dispatch-a", 1002, 2000)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claim.row.artifact_id, a.artifact_id);
    assert!(mark_dispatched(&db, &claim, 1, 1003).await.unwrap());
    let in_flight = load(&db, "owner", &a.artifact_id).await.unwrap().unwrap();
    lifecycle::cancel_request_on(
        &db,
        "owner",
        "continuous-conversation",
        2,
        "work_stopped",
        1004,
    )
    .await
    .unwrap();
    assert_eq!(
        load(&db, "owner", &a.artifact_id).await.unwrap().unwrap(),
        in_flight
    );
    let cancelled = load(&db, "owner", &b.artifact_id).await.unwrap().unwrap();
    assert_eq!(cancelled.state, NOT_SENT);
    assert_eq!(decode(&cancelled).unwrap(), b);
    assert!(
        record_receipt(&db, &claim, "dispatch-a", &receipt(&a, 9), 1005)
            .await
            .unwrap()
    );
    assert_eq!(
        load(&db, "owner", &a.artifact_id)
            .await
            .unwrap()
            .unwrap()
            .state,
        DELIVERED
    );
}
