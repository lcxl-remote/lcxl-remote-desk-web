use super::*;
mod retained;
use super::super::source_lifecycle::{
    conversation_readable_on, mark_manual_delete_on, origin_readable_on,
};
use crate::entity::{
    agent_assistant_history as archive, agent_assistant_origin as origin,
    agent_file_recovery_cleanup as removal, agent_memory_source as evidence,
};
use desk_agent_protocol::ai_assistant::{
    history::HistoryAdmission,
    memory::MemoryListRequest,
    source::{AssistantSourceKind, HistoryDependency, SourceAvailability},
};
use desk_diagnose_core::{
    ai_assistant::source::{HistoryCapture, owner_text_policy},
    model_message_labels::model_bound_user_message,
    native_ids::{self, Kind},
};
use sea_orm::{PaginatorTrait, sea_query::Expr};

async fn native_removal_on<C: ConnectionTrait>(db: &C, conversation: &str, actor: &str) {
    removal::ActiveModel {
        conversation_id: Set(conversation.into()),
        actor_id: Set(actor.into()),
        device_id: Set("native-device".into()),
        created_at_unix_ms: Set(100),
        history_deleted_at_unix_ms: Set(None),
        next_attempt_at_unix_ms: Set(100),
        attempts: Set(0),
        completed_at_unix_ms: Set(Some(200)),
        ..Default::default()
    }
    .insert(db)
    .await
    .unwrap();
}

async fn remove(store: &TestStore, conversation: &str, now: i64) {
    let txn = crate::db::begin_write(&store.db, removal::Entity)
        .await
        .unwrap();
    mark_manual_delete_on(&txn, "1", "native-device", conversation, now)
        .await
        .unwrap();
    txn.commit().await.unwrap();
}

#[tokio::test]
async fn natural_execution_reclamation_and_file_cleanup_do_not_expire_independent_facts_or_history()
{
    let store = setup().await;
    let (memory, evidence_id, original_id) = super::owner_memory::seed(&store, 1, "natural").await;
    let original = origin::Entity::find()
        .filter(origin::Column::Id.eq(original_id))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let history_id = super::keyword_search::capture(
        &store,
        &original.device_ref,
        &original.conversation_id,
        "retained-original",
        "Original research report",
        &[],
    )
    .await;
    let before = runtime_on(&store.db, "1").await.unwrap();
    native_removal_on(&store.db, &original.conversation_id, "1").await;
    assert!(
        conversation_readable_on(
            &store.db,
            "1",
            &original.conversation_id,
            &original.root_conversation_id
        )
        .await
        .unwrap()
    );
    assert!(origin_readable_on(&store.db, "1", &original).await.unwrap());
    assert_eq!(runtime_on(&store.db, "1").await.unwrap(), before);
    let policy = AssistantDataPolicy::default();
    assert!(
        store
            .memories(1, &MemoryListRequest::default(), &policy, 1000)
            .await
            .unwrap()
            .memories
            .iter()
            .any(|row| row.memory_id == memory)
    );
    assert_ne!(
        store
            .owner_source(1, &history_id, &policy, 1000)
            .await
            .unwrap()
            .availability,
        SourceAvailability::Unavailable
    );
    let evidence = evidence::Entity::find()
        .filter(
            evidence::Column::Id.eq(desk_assistant_data_store::source_rows::memory(evidence_id)),
        )
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    assert!(
        super::super::evidence::allowed_on(&store.db, "1", &evidence, 91 * 86400 * 1000)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn manual_root_deletion_denies_completed_child_evidence_and_derived_text_without_affecting_other_roots()
 {
    let store = setup().await;
    let (deleted_memory, evidence_id, origin_id) =
        super::owner_memory::seed(&store, 1, "child").await;
    let (_, _, sibling_origin) = super::owner_memory::seed(&store, 1, "other").await;
    origin::Entity::update_many()
        .col_expr(
            origin::Column::ConversationId,
            Expr::value("completed-child"),
        )
        .col_expr(
            origin::Column::RootConversationId,
            Expr::value("removed-root"),
        )
        .filter(origin::Column::Id.eq(origin_id))
        .exec(&store.db)
        .await
        .unwrap();
    super::owner_memory::rebuild_evidence_proof(&store, &evidence_id).await;
    let original = origin::Entity::find()
        .filter(origin::Column::Id.eq(origin_id))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let sibling = origin::Entity::find()
        .filter(origin::Column::Id.eq(sibling_origin))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let dependency = HistoryDependency {
        source_id: evidence_id.clone(),
        source_revision: 1,
        origin_id,
        origin_deletion_generation: unsigned(original.deletion_generation).unwrap(),
        policy_revision: 1,
        expires_at_unix_ms: None,
    };
    let derived = super::keyword_search::capture(
        &store,
        &sibling.device_ref,
        "other-root",
        "derived-copy",
        "Answer based on the original preference",
        &[dependency],
    )
    .await;
    let policy = AssistantDataPolicy::default();
    assert_ne!(
        store
            .owner_source(1, &derived, &policy, 1000)
            .await
            .unwrap()
            .availability,
        SourceAvailability::Unavailable
    );
    let mut usage = super::model_sources::memory_usage(&store, &deleted_memory, &evidence_id).await;
    let before_access = super::model_sources::access(&store).await;
    assert!(
        super::super::model_sources::validate_on(
            &store.db,
            "1",
            "current-device",
            &usage,
            &before_access,
            None,
            &super::model_sources::destination("original-model"),
            &policy,
            1000
        )
        .await
        .is_ok()
    );
    let before = runtime_on(&store.db, "1").await.unwrap();
    native_removal_on(&store.db, "removed-root", "1").await;
    remove(&store, "removed-root", 1200).await;
    let after = runtime_on(&store.db, "1").await.unwrap();
    assert_eq!(
        after.history_data_generation,
        before.history_data_generation + 1
    );
    assert_eq!(
        after.memory_state_generation,
        before.memory_state_generation + 1
    );
    assert_eq!(after.control_generation, before.control_generation + 1);

    assert_eq!(after.input_seq, before.input_seq + 1);
    assert!(!origin_readable_on(&store.db, "1", &original).await.unwrap());
    assert!(origin_readable_on(&store.db, "1", &sibling).await.unwrap());
    assert!(
        !store
            .memories(1, &MemoryListRequest::default(), &policy, 1300)
            .await
            .unwrap()
            .memories
            .iter()
            .any(|row| row.memory_id == deleted_memory)
    );
    assert_eq!(
        store
            .owner_source(1, &derived, &policy, 1300)
            .await
            .unwrap()
            .availability,
        SourceAvailability::Unavailable
    );
    let access = super::model_sources::access(&store).await;
    usage.control_generation = access.control_generation;
    usage.memory_state_generation = access.memory_state_generation;
    assert!(
        super::super::model_sources::validate_on(
            &store.db,
            "1",
            "current-device",
            &usage,
            &access,
            None,
            &super::model_sources::destination("another-model"),
            &policy,
            1300
        )
        .await
        .is_err()
    );
    // Logical revocation does not wait for physical cleanup or rewrite evidence.
    assert!(
        origin::Entity::find()
            .filter(origin::Column::Id.eq(origin_id))
            .one(&store.db)
            .await
            .unwrap()
            .unwrap()
            .deleted_at_ms
            .is_none()
    );
    assert!(
        !evidence::Entity::find()
            .filter(
                evidence::Column::Id
                    .eq(desk_assistant_data_store::source_rows::memory(evidence_id))
            )
            .one(&store.db)
            .await
            .unwrap()
            .unwrap()
            .body
            .is_empty()
    );
}

#[tokio::test]
async fn missing_history_at_deletion_cannot_be_recreated_by_a_late_native_capture() {
    let store = setup().await;
    store
        .settings(1, &AssistantDataPolicy::default())
        .await
        .unwrap();
    native_removal_on(&store.db, "never-indexed-root", "1").await;
    remove(&store, "never-indexed-root", 1200).await;
    let message = model_bound_user_message(
        native_ids::stable(Kind::Message, &"late"),
        "A late visible answer".into(),
        super::model_sources::destination("model"),
    )
    .unwrap();
    let policy = owner_text_policy(1, false);
    let state = runtime_on(&store.db, "1").await.unwrap();
    let instance = store.db.config_read().await.metadata.instance.clone();
    let audience = "original-source-device";
    let device_ref = native_ids::stable(Kind::AssistantDeviceBinding, &(&instance, audience));
    let txn = crate::db::begin_write(&store.db, removal::Entity)
        .await
        .unwrap();
    let mut capture = HistoryCapture {
        native: None,
        actor_id: "1",
        device_ref: &device_ref,
        directory: Some(
            desk_diagnose_core::ai_assistant::source::HistorySourceDirectory {
                instance_id: &instance,
                audience,
                device_name: None,
            },
        ),
        conversation_id: "completed-child",
        root_conversation_id: "never-indexed-root",
        native_id: &message.message_id,
        kind: AssistantSourceKind::Input,
        role: "user",
        content: &message.text,
        envelope: message.data_envelope.as_ref().unwrap(),
        source_policy: &policy,
        dependencies: &[],
        accepted_input_seq: unsigned(state.input_seq).unwrap(),
        recorded_at_unix_ms: 1100,
        statement_timezone: "UTC",
        revision: 1,
    };
    let outcome =
        super::super::history::capture_on(&txn, &capture, &AssistantDataPolicy::default())
            .await
            .unwrap();
    assert_eq!(outcome.admission, HistoryAdmission::OmittedPolicy);
    assert!(outcome.source_ids.is_empty());
    txn.commit().await.unwrap();
    let txn = crate::db::begin_write(&store.db, removal::Entity)
        .await
        .unwrap();
    capture.conversation_id = "unrelated-root";
    capture.root_conversation_id = "unrelated-root";
    let positive =
        super::super::history::capture_on(&txn, &capture, &AssistantDataPolicy::default())
            .await
            .unwrap();
    assert_eq!(positive.admission, HistoryAdmission::Indexed);
    txn.commit().await.unwrap();
    assert_eq!(archive::Entity::find().count(&store.db).await.unwrap(), 1);
    assert_eq!(
        origin::Entity::find()
            .filter(origin::Column::RootConversationId.eq("never-indexed-root"))
            .count(&store.db)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn manual_deletion_rolls_back_atomically_and_a_retry_does_not_advance_generations_again() {
    let store = setup().await;
    store
        .settings(1, &AssistantDataPolicy::default())
        .await
        .unwrap();
    native_removal_on(&store.db, "root", "1").await;
    let before = runtime_on(&store.db, "1").await.unwrap();
    let txn = crate::db::begin_write(&store.db, removal::Entity)
        .await
        .unwrap();
    mark_manual_delete_on(&txn, "1", "native-device", "root", 1200)
        .await
        .unwrap();
    assert!(
        !conversation_readable_on(&txn, "1", "root", "root")
            .await
            .unwrap()
    );
    txn.rollback().await.unwrap();
    assert!(
        conversation_readable_on(&store.db, "1", "root", "root")
            .await
            .unwrap()
    );
    assert_eq!(runtime_on(&store.db, "1").await.unwrap(), before);
    remove(&store, "root", 1200).await;
    let applied = runtime_on(&store.db, "1").await.unwrap();
    remove(&store, "root", 1300).await;
    assert_eq!(runtime_on(&store.db, "1").await.unwrap(), applied);
    let marker = removal::Entity::find_by_id("root")
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(marker.history_deleted_at_unix_ms, Some(1200));
    assert_eq!(marker.completed_at_unix_ms, Some(200));
}

#[tokio::test]
async fn real_file_cleanup_completion_and_native_tombstone_replay_preserve_manual_history_deletion()
{
    let store = setup().await;
    store
        .settings(1, &AssistantDataPolicy::default())
        .await
        .unwrap();
    native_removal_on(&store.db, "root", "1").await;
    removal::Entity::update_many()
        .col_expr(
            removal::Column::CompletedAtUnixMs,
            Expr::value(Option::<i64>::None),
        )
        .filter(removal::Column::ConversationId.eq("root"))
        .exec(&store.db)
        .await
        .unwrap();
    let files = crate::file_recovery_cleanup_store::FileRecoveryCleanupStore::new(store.db.clone());
    let claimed = files.claim_due(1000).await.unwrap().unwrap();
    remove(&store, "root", 1100).await;
    assert!(
        files
            .complete("root", claimed.lease_id.as_deref().unwrap(), 1200)
            .await
            .unwrap()
    );
    let txn = crate::db::begin_write(&store.db, removal::Entity)
        .await
        .unwrap();
    crate::agent_subagent_store::tombstone_on(&txn, "root", "1", "native-device", 1300)
        .await
        .unwrap();
    txn.commit().await.unwrap();
    let row = removal::Entity::find_by_id("root")
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.history_deleted_at_unix_ms, Some(1100));
    assert_eq!(row.completed_at_unix_ms, Some(1200));
    assert_eq!(row.attempts, 1);
    assert!(
        !conversation_readable_on(&store.db, "1", "child", "root")
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn a_known_original_origin_cannot_be_hidden_behind_a_new_primary_evidence_body() {
    use desk_diagnose_core::ai_assistant::memory_control::{KnownMemorySource, canonical_sources};
    let store = setup().await;
    let (_, old_source, old_origin) = super::owner_memory::seed(&store, 1, "old-evidence").await;
    let (_, newer_source, newer_origin) =
        super::owner_memory::seed(&store, 1, "new-evidence").await;
    let old = evidence::Entity::find()
        .filter(
            evidence::Column::Id.eq(desk_assistant_data_store::source_rows::memory(&old_source)),
        )
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let current = evidence::Entity::find()
        .filter(
            evidence::Column::Id.eq(desk_assistant_data_store::source_rows::memory(
                &newer_source,
            )),
        )
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let mut known: Vec<KnownMemorySource> =
        serde_json::from_str(&current.known_sources_json).unwrap();
    let old_known: Vec<KnownMemorySource> = serde_json::from_str(&old.known_sources_json).unwrap();
    let original = origin::Entity::find()
        .filter(origin::Column::Id.eq(old_origin))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let original_policy: desk_agent_protocol::ai_assistant::source::SourceUsePolicy =
        serde_json::from_str(&original.policy_json).unwrap();
    let mut dependencies: Vec<HistoryDependency> =
        serde_json::from_str(&current.dependencies_json).unwrap();
    for source in &old_known {
        dependencies.push(HistoryDependency {
            source_id: source.source_id.clone(),
            source_revision: source.revision,
            origin_id: source.origin_id,
            origin_deletion_generation: original.deletion_generation as u64,
            policy_revision: original_policy.policy_revision,
            expires_at_unix_ms: original_policy.content_hard_expires_at_unix_ms,
        });
    }
    known.extend(old_known);
    let known = canonical_sources(known).unwrap();
    evidence::Entity::update_many()
        .col_expr(
            evidence::Column::KnownSourcesJson,
            Expr::value(serde_json::to_string(&known).unwrap()),
        )
        .col_expr(
            evidence::Column::DependenciesJson,
            Expr::value(serde_json::to_string(&dependencies).unwrap()),
        )
        .filter(
            evidence::Column::Id.eq(desk_assistant_data_store::source_rows::memory(
                &newer_source,
            )),
        )
        .exec(&store.db)
        .await
        .unwrap();
    super::owner_memory::rebuild_evidence_proof(&store, &newer_source).await;
    let current = evidence::Entity::find()
        .filter(
            evidence::Column::Id.eq(desk_assistant_data_store::source_rows::memory(
                &newer_source,
            )),
        )
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    assert!(
        super::super::evidence::allowed_on(&store.db, "1", &current, 1000)
            .await
            .unwrap()
    );
    let old_origin = origin::Entity::find()
        .filter(origin::Column::Id.eq(old_origin))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    native_removal_on(&store.db, &old_origin.root_conversation_id, "1").await;
    remove(&store, &old_origin.root_conversation_id, 1200).await;
    let newer_origin = origin::Entity::find()
        .filter(origin::Column::Id.eq(newer_origin))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    assert!(
        origin_readable_on(&store.db, "1", &newer_origin)
            .await
            .unwrap()
    );
    assert!(
        !super::super::evidence::allowed_on(&store.db, "1", &current, 1300)
            .await
            .unwrap()
    );
    assert!(
        !super::super::evidence::owner_history_identity_allowed_on(&store.db, "1", &current, 1300)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn conflicting_native_subjects_and_wrong_device_cannot_mark_or_reuse_history() {
    let store = setup().await;
    native_removal_on(&store.db, "foreign-root", "other-owner").await;
    assert!(
        !conversation_readable_on(&store.db, "1", "child", "foreign-root")
            .await
            .unwrap()
    );
    let txn = crate::db::begin_write(&store.db, removal::Entity)
        .await
        .unwrap();
    assert!(matches!(
        mark_manual_delete_on(&txn, "1", "native-device", "foreign-root", 1200).await,
        Err(AssistantStoreError::Inaccessible)
    ));
    txn.rollback().await.unwrap();
    native_removal_on(&store.db, "own-root", "1").await;
    let txn = crate::db::begin_write(&store.db, removal::Entity)
        .await
        .unwrap();
    assert!(
        mark_manual_delete_on(&txn, "1", "wrong-device", "own-root", 1200)
            .await
            .is_err()
    );
    txn.rollback().await.unwrap();
    assert!(
        conversation_readable_on(&store.db, "1", "own-root", "own-root")
            .await
            .unwrap()
    );
}
