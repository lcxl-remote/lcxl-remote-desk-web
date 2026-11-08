use super::model_sources::{access, destination, history_usage, memory_usage};
use super::*;
use crate::entity::{
    agent_assistant_origin as origin, agent_memory as memory, agent_memory_revision as revision,
    agent_memory_source as evidence,
};
use desk_agent_protocol::ai_assistant::{
    memory::{MemoryDeleteRequest, MemoryUsePurpose},
    source::{AssistantSourceKind, HistoryDependency, SourceAvailability, SourceUsePolicy},
};
use desk_diagnose_core::{
    ai_assistant::{
        data_access::owner_history_scope,
        source::{HistoryCapture, derive_visible_text_policy},
    },
    model_message_labels::model_bound_user_message,
    native_ids::{self, Kind},
};

const DAY: u64 = 86_400_000;

async fn evidence_dependency(store: &TestStore, source: &str) -> HistoryDependency {
    let row = evidence::Entity::find()
        .filter(evidence::Column::SourceId.eq(source))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let original = origin::Entity::find()
        .filter(origin::Column::OriginId.eq(&row.origin_id))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let policy: SourceUsePolicy = serde_json::from_str(&row.policy_json).unwrap();
    HistoryDependency {
        source_id: source.into(),
        source_revision: row.memory_revision as u64,
        origin_id: row.origin_id,
        origin_deletion_generation: original.deletion_generation as u64,
        policy_revision: policy.policy_revision,
        expires_at_unix_ms: row.expires_at_ms.map(|value| value as u64),
    }
}

// This fixture archives a policy-bound visible answer at a later native input.
// It tests storage/provenance only, without simulating provider dispatch authority.
async fn archive_copy(store: &TestStore, source: &str, dependency: &HistoryDependency) -> String {
    let retained = evidence::Entity::find()
        .filter(evidence::Column::SourceId.eq(source))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let original = origin::Entity::find()
        .filter(origin::Column::OriginId.eq(&retained.origin_id))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let parent: SourceUsePolicy = serde_json::from_str(&retained.policy_json).unwrap();
    let policy = derive_visible_text_policy(&[parent], 1).unwrap();
    let actor = "1";
    let conversation = "retained-answer";
    let native = native_ids::stable(Kind::Message, &"retained-answer");
    let body = "Previously recorded preference: Chinese.";
    let message =
        model_bound_user_message(native.clone(), body.into(), destination("model")).unwrap();
    let envelope = message.data_envelope.unwrap();
    let state = runtime_on(&store.db, actor).await.unwrap();
    let binding = crate::entity::agent_assistant_device_binding::Entity::find()
        .filter(
            crate::entity::agent_assistant_device_binding::Column::BindingId
                .eq(&original.device_ref),
        )
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let directory = Some(
        desk_diagnose_core::ai_assistant::source::HistorySourceDirectory {
            instance_id: &binding.instance_id,
            audience: &binding.audience,
            device_name: binding.device_name.as_deref(),
        },
    );
    let capture = HistoryCapture {
        native: None,
        actor_id: actor,
        device_ref: &original.device_ref,
        directory,
        conversation_id: conversation,
        root_conversation_id: conversation,
        native_id: &native,
        accepted_input_seq: state.input_seq as u64,
        kind: AssistantSourceKind::AssistantMessage,
        role: "assistant",
        content: body,
        envelope: &envelope,
        source_policy: &policy,
        dependencies: std::slice::from_ref(dependency),
        recorded_at_unix_ms: 20 * DAY,
        statement_timezone: "UTC",
        revision: 1,
    };
    let txn = store.db.begin().await.unwrap();
    let outcome =
        super::super::history::capture_on(&txn, &capture, &AssistantDataPolicy::default())
            .await
            .unwrap();
    assert_eq!(outcome.source_ids.len(), 1);
    txn.commit().await.unwrap();
    outcome.source_ids[0].clone()
}

async fn natural_body_cleanup(store: &TestStore, memory_id: &str, source: &str) {
    memory::Entity::update_many()
        .col_expr(
            memory::Column::Status,
            sea_orm::sea_query::Expr::value("superseded"),
        )
        .filter(memory::Column::MemoryId.eq(memory_id))
        .exec(&store.db)
        .await
        .unwrap();
    revision::Entity::update_many()
        .col_expr(
            revision::Column::Status,
            sea_orm::sea_query::Expr::value("superseded"),
        )
        .col_expr(
            revision::Column::ExpiresAtMs,
            sea_orm::sea_query::Expr::value(Some((30 * DAY) as i64)),
        )
        .col_expr(revision::Column::Body, sea_orm::sea_query::Expr::value(""))
        .col_expr(
            revision::Column::Context,
            sea_orm::sea_query::Expr::value(None::<String>),
        )
        .filter(revision::Column::MemoryId.eq(memory_id))
        .exec(&store.db)
        .await
        .unwrap();
    evidence::Entity::update_many()
        .col_expr(evidence::Column::Body, sea_orm::sea_query::Expr::value(""))
        .col_expr(
            evidence::Column::BodyClearedAtMs,
            sea_orm::sea_query::Expr::value(Some((31 * DAY) as i64)),
        )
        .filter(evidence::Column::SourceId.eq(source))
        .exec(&store.db)
        .await
        .unwrap();
}

#[tokio::test]
async fn a_retained_answer_survives_natural_revision_body_cleanup_but_not_model_forgetting() {
    let store = setup().await;
    let (memory, source, _) = super::owner_memory::seed(&store, 1, "copy").await;
    let dependency = evidence_dependency(&store, &source).await;
    let answer = archive_copy(&store, &source, &dependency).await;
    natural_body_cleanup(&store, &memory, &source).await;
    let (device, mut usage) = history_usage(&store, &answer).await;
    usage.history_dependencies.push(dependency);
    let state = access(&store).await;
    let scope = owner_history_scope(
        "1",
        "root",
        &device,
        10,
        20 * DAY,
        "episode",
        1,
        &state.settings,
        "UTC",
        false,
    )
    .unwrap()
    .unwrap();
    let policy = AssistantDataPolicy::default();
    super::super::model_sources::validate_on(
        &store.db,
        "1",
        &device,
        &usage,
        &state,
        Some(&scope),
        &destination("model"),
        &policy,
        40 * DAY,
    )
    .await
    .unwrap();
    let mut direct = memory_usage(&store, &memory, &source).await;
    direct.memories[0].purpose = MemoryUsePurpose::HistoricalReference;
    assert!(
        super::super::model_sources::validate_on(
            &store.db,
            "1",
            &device,
            &direct,
            &state,
            None,
            &destination("model"),
            &policy,
            40 * DAY
        )
        .await
        .is_err()
    );
    assert_eq!(
        store
            .owner_source(1, &answer, &policy, 40 * DAY)
            .await
            .unwrap()
            .availability,
        SourceAvailability::Available
    );
    assert_eq!(
        store
            .owner_source(1, &source, &policy, 40 * DAY)
            .await
            .unwrap()
            .availability,
        SourceAvailability::Unavailable
    );

    store
        .forget_memory(
            1,
            &MemoryDeleteRequest {
                memory_id: memory,
                expected_revision: 1,
                operation_id: "forget-copy-source".into(),
            },
            &policy,
            41 * DAY,
        )
        .await
        .unwrap();
    let state = access(&store).await;
    usage.control_generation = state.control_generation;
    usage.memory_state_generation = state.memory_state_generation;
    assert!(
        super::super::model_sources::validate_on(
            &store.db,
            "1",
            &device,
            &usage,
            &state,
            Some(&scope),
            &destination("model"),
            &policy,
            41 * DAY + 1
        )
        .await
        .is_err()
    );
    assert_eq!(
        store
            .owner_source(1, &answer, &policy, 41 * DAY + 1)
            .await
            .unwrap()
            .availability,
        SourceAvailability::Available
    );
    assert_eq!(
        store
            .owner_source(1, &source, &policy, 41 * DAY + 1)
            .await
            .unwrap()
            .availability,
        SourceAvailability::Unavailable
    );
}

#[tokio::test]
async fn a_new_fact_keeps_its_full_producer_constraints_after_the_old_fact_body_is_cleaned() {
    let store = setup().await;
    let (old_memory, old_source, _) = super::owner_memory::seed(&store, 1, "older-fact").await;
    let row = evidence::Entity::find()
        .filter(evidence::Column::SourceId.eq(&old_source))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let mut policy: SourceUsePolicy = serde_json::from_str(&row.policy_json).unwrap();
    policy.policy_revision = 2;
    evidence::Entity::update_many()
        .col_expr(
            evidence::Column::PolicyJson,
            sea_orm::sea_query::Expr::value(serde_json::to_string(&policy).unwrap()),
        )
        .filter(evidence::Column::SourceId.eq(&old_source))
        .exec(&store.db)
        .await
        .unwrap();
    super::owner_memory::rebuild_evidence_proof(&store, &old_source).await;
    let dependency = evidence_dependency(&store, &old_source).await;
    let (new_memory, new_source, _) = super::owner_memory::seed(&store, 1, "newer-fact").await;
    let dependencies = serde_json::to_string(&vec![dependency.clone()]).unwrap();
    evidence::Entity::update_many()
        .col_expr(
            evidence::Column::DependenciesJson,
            sea_orm::sea_query::Expr::value(dependencies.clone()),
        )
        .filter(evidence::Column::SourceId.eq(&new_source))
        .exec(&store.db)
        .await
        .unwrap();
    revision::Entity::update_many()
        .col_expr(
            revision::Column::DependenciesJson,
            sea_orm::sea_query::Expr::value(dependencies),
        )
        .filter(revision::Column::MemoryId.eq(&new_memory))
        .exec(&store.db)
        .await
        .unwrap();
    super::owner_memory::rebuild_evidence_proof(&store, &new_source).await;
    natural_body_cleanup(&store, &old_memory, &old_source).await;
    let mut usage = memory_usage(&store, &new_memory, &new_source).await;
    usage.history_dependencies.push(dependency);
    let state = access(&store).await;
    let platform = AssistantDataPolicy::default();
    assert!(
        super::super::model_sources::validate_on(
            &store.db,
            "1",
            "current-device",
            &usage,
            &state,
            None,
            &destination("model"),
            &platform,
            40 * DAY
        )
        .await
        .unwrap()
        .is_none()
    );
    store
        .forget_memory(
            1,
            &MemoryDeleteRequest {
                memory_id: old_memory,
                expected_revision: 1,
                operation_id: "forget-old-producer".into(),
            },
            &platform,
            41 * DAY,
        )
        .await
        .unwrap();
    let state = access(&store).await;
    usage.control_generation = state.control_generation;
    usage.memory_state_generation = state.memory_state_generation;
    assert!(
        super::super::model_sources::validate_on(
            &store.db,
            "1",
            "current-device",
            &usage,
            &state,
            None,
            &destination("model"),
            &platform,
            41 * DAY + 1
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn identity_cleanup_is_valid_only_after_the_original_body_retention_boundary() {
    let store = setup().await;
    let (memory, source, origin_id) = super::owner_memory::seed(&store, 1, "cleanup-proof").await;
    natural_body_cleanup(&store, &memory, &source).await;
    let row = evidence::Entity::find()
        .filter(evidence::Column::SourceId.eq(&source))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    assert!(
        super::super::evidence::load_on(&store.db, "1", &row, true, None, 40 * DAY)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        !super::super::evidence::allowed_on(&store.db, "1", &row, 40 * DAY)
            .await
            .unwrap()
    );
    let mut corrupted = row.clone();
    corrupted.body_cleared_at_ms = None;
    assert!(
        super::super::evidence::load_on(&store.db, "1", &corrupted, true, None, 40 * DAY)
            .await
            .unwrap()
            .is_none()
    );
    let mut corrupted = row.clone();
    corrupted.body_cleared_at_ms = Some((29 * DAY) as i64);
    assert!(
        super::super::evidence::load_on(&store.db, "1", &corrupted, true, None, 40 * DAY)
            .await
            .unwrap()
            .is_none()
    );
    origin::Entity::update_many()
        .col_expr(
            origin::Column::DeletedAtMs,
            sea_orm::sea_query::Expr::value(Some((40 * DAY) as i64)),
        )
        .filter(origin::Column::OriginId.eq(origin_id))
        .exec(&store.db)
        .await
        .unwrap();
    assert!(
        super::super::evidence::load_on(&store.db, "1", &row, true, None, 40 * DAY)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn retained_evidence_policy_has_its_own_revision_and_real_hard_deadline() {
    let store = setup().await;
    let (memory, source, origin_id) = super::owner_memory::seed(&store, 1, "hard-copy").await;
    let row = evidence::Entity::find()
        .filter(evidence::Column::SourceId.eq(&source))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let mut source_policy: SourceUsePolicy = serde_json::from_str(&row.policy_json).unwrap();
    source_policy.policy_revision = 2;
    source_policy.content_hard_expires_at_unix_ms = Some(50 * DAY);
    evidence::Entity::update_many()
        .col_expr(
            evidence::Column::PolicyJson,
            sea_orm::sea_query::Expr::value(serde_json::to_string(&source_policy).unwrap()),
        )
        .col_expr(
            evidence::Column::ExpiresAtMs,
            sea_orm::sea_query::Expr::value(Some((50 * DAY) as i64)),
        )
        .filter(evidence::Column::SourceId.eq(&source))
        .exec(&store.db)
        .await
        .unwrap();
    super::owner_memory::rebuild_evidence_proof(&store, &source).await;
    let dependency = evidence_dependency(&store, &source).await;
    assert_eq!(dependency.policy_revision, 2);
    let original = origin::Entity::find()
        .filter(origin::Column::OriginId.eq(origin_id))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::from_str::<SourceUsePolicy>(&original.policy_json)
            .unwrap()
            .policy_revision,
        1
    );
    let answer = archive_copy(&store, &source, &dependency).await;
    natural_body_cleanup(&store, &memory, &source).await;
    let (device, mut usage) = history_usage(&store, &answer).await;
    usage.history_dependencies.push(dependency);
    let state = access(&store).await;
    let scope = owner_history_scope(
        "1",
        "root",
        &device,
        10,
        20 * DAY,
        "episode",
        1,
        &state.settings,
        "UTC",
        false,
    )
    .unwrap()
    .unwrap();
    let platform = AssistantDataPolicy::default();
    assert_eq!(
        super::super::model_sources::validate_on(
            &store.db,
            "1",
            &device,
            &usage,
            &state,
            Some(&scope),
            &destination("model"),
            &platform,
            40 * DAY
        )
        .await
        .unwrap(),
        Some(50 * DAY)
    );
    assert_eq!(
        store
            .owner_source(1, &answer, &platform, 40 * DAY)
            .await
            .unwrap()
            .availability,
        SourceAvailability::Available
    );
    assert!(
        super::super::model_sources::validate_on(
            &store.db,
            "1",
            &device,
            &usage,
            &state,
            Some(&scope),
            &destination("model"),
            &platform,
            50 * DAY
        )
        .await
        .is_err()
    );
}
