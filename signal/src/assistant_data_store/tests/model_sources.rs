use super::super::model_sources::validate_on;
use super::*;
use crate::entity::{
    agent_assistant_history as history, agent_assistant_origin as origin, agent_memory as fact,
    agent_memory_revision as version, agent_memory_source as evidence,
};
use desk_agent_protocol::{
    ai_assistant::{
        memory::{MemoryDeleteRequest, MemoryUsePurpose},
        source::{HistoryDependency, SourceAvailability, SourceReadPurpose, SourceUsePolicy},
    },
    data_lineage::DestinationIdentity,
};
use desk_diagnose_core::{
    ai_assistant::{
        data_access::{AssistantDataToolAccess, owner_history_scope},
        data_control::CurrentMemoryUse,
        seam::{AssistantDataReadUse, AssistantReadRoot},
    },
    native_ids::{self, Kind},
};

pub(super) fn destination(model: &str) -> DestinationIdentity {
    DestinationIdentity::Model {
        connection_id: "gateway".into(),
        connection_revision: 1,
        model_id: model.into(),
        profile_revision: 1,
    }
}

pub(super) async fn access(store: &TestStore) -> AssistantDataToolAccess {
    let state = runtime_on(&store.db, "1").await.unwrap();
    AssistantDataToolAccess {
        settings: store
            .settings(1, &AssistantDataPolicy::default())
            .await
            .unwrap(),
        policy_revision: 1,
        memory_state_generation: unsigned(state.memory_state_generation).unwrap(),
        control_generation: unsigned(state.control_generation).unwrap(),
    }
}

pub(super) async fn memory_usage(
    store: &TestStore,
    memory: &str,
    source: &str,
) -> AssistantDataReadUse {
    let data = access(store).await;
    let evidence = evidence::Entity::find()
        .filter(evidence::Column::Id.eq(desk_assistant_data_store::source_rows::memory(source)))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let original = origin::Entity::find()
        .filter(origin::Column::OriginId.eq(&evidence.origin_id))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    AssistantDataReadUse {
        decision_roots: Vec::new(),
        projection_id: native_ids::stable(Kind::AssistantReadProjection, &source),
        source_envelope_id: "memory-read-result".into(),
        purpose: SourceReadPurpose::ModelMemory,
        retrieval_episode_id: "episode".into(),
        scope: Box::default(),
        policy_revision: 1,
        control_generation: data.control_generation,
        memory_state_generation: data.memory_state_generation,
        memories: vec![CurrentMemoryUse {
            memory_id: memory.into(),
            revision: 1,
            purpose: MemoryUsePurpose::CurrentFact,
        }],
        roots: vec![AssistantReadRoot {
            source_id: source.into(),
            source_revision: 1,
        }],
        history_dependencies: vec![HistoryDependency {
            source_id: source.into(),
            source_revision: 1,
            origin_id: evidence.origin_id,
            origin_deletion_generation: unsigned(original.deletion_generation).unwrap(),
            policy_revision: 1,
            expires_at_unix_ms: None,
        }],
        expires_at_unix_ms: None,
    }
}

pub(super) async fn history_usage(
    store: &TestStore,
    source: &str,
) -> (String, AssistantDataReadUse) {
    let state = access(store).await;
    let row = history::Entity::find()
        .filter(history::Column::Id.eq(desk_assistant_data_store::source_rows::history(source)))
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
    (
        row.device_ref,
        AssistantDataReadUse {
            decision_roots: Vec::new(),
            projection_id: native_ids::stable(Kind::AssistantReadProjection, &source),
            source_envelope_id: "history-read-result".into(),
            purpose: SourceReadPurpose::ModelHistory,
            retrieval_episode_id: "episode".into(),
            scope: Box::default(),
            policy_revision: 1,
            control_generation: state.control_generation,
            memory_state_generation: state.memory_state_generation,
            memories: Vec::new(),
            roots: vec![AssistantReadRoot {
                source_id: source.into(),
                source_revision: 1,
            }],
            history_dependencies: vec![HistoryDependency {
                source_id: source.into(),
                source_revision: 1,
                origin_id: row.origin_id,
                origin_deletion_generation: unsigned(original.deletion_generation).unwrap(),
                policy_revision: 1,
                expires_at_unix_ms: Some(row.expires_at_ms as u64),
            }],
            expires_at_unix_ms: Some(row.expires_at_ms as u64),
        },
    )
}

#[tokio::test]
async fn independent_fact_remains_model_neutral_after_natural_history_expiry() {
    let store = setup().await;
    let (memory, source, _) = super::owner_memory::seed(&store, 1, "neutral").await;
    let original_id = super::owner_memory::seed_original_history(&store, &source).await;
    let usage = memory_usage(&store, &memory, &source).await;
    let access = access(&store).await;
    let platform = AssistantDataPolicy::default();
    let now = 91 * 86_400_000;
    assert!(
        validate_on(
            &store.db,
            "1",
            "current-device",
            &usage,
            &access,
            None,
            &destination("model-a"),
            &platform,
            now
        )
        .await
        .unwrap()
        .is_none()
    );
    assert!(
        validate_on(
            &store.db,
            "1",
            "current-device",
            &usage,
            &access,
            None,
            &destination("model-b"),
            &platform,
            now
        )
        .await
        .unwrap()
        .is_none()
    );
    let (device, history) = history_usage(&store, &original_id).await;
    let scope = owner_history_scope(
        "1",
        "root",
        &device,
        10,
        100,
        "episode",
        1,
        &access.settings,
        "UTC",
        false,
    )
    .unwrap()
    .unwrap();
    assert!(
        validate_on(
            &store.db,
            "1",
            &device,
            &history,
            &access,
            Some(&scope),
            &destination("model-b"),
            &platform,
            now
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn superseded_memory_is_historical_and_gets_its_real_body_retention_deadline() {
    let store = setup().await;
    let (memory, source, _) = super::owner_memory::seed(&store, 1, "superseded").await;
    let mut usage = memory_usage(&store, &memory, &source).await;
    fact::Entity::update_many()
        .col_expr(
            fact::Column::Status,
            sea_orm::sea_query::Expr::value("superseded"),
        )
        .filter(fact::Column::MemoryId.eq(&memory))
        .exec(&store.db)
        .await
        .unwrap();
    version::Entity::update_many()
        .col_expr(
            version::Column::Status,
            sea_orm::sea_query::Expr::value("superseded"),
        )
        .col_expr(
            version::Column::ExpiresAtMs,
            sea_orm::sea_query::Expr::value(Some(2000_i64)),
        )
        .filter(version::Column::MemoryId.eq(&memory))
        .exec(&store.db)
        .await
        .unwrap();
    let access = access(&store).await;
    let policy = AssistantDataPolicy::default();
    assert!(
        validate_on(
            &store.db,
            "1",
            "current-device",
            &usage,
            &access,
            None,
            &destination("model"),
            &policy,
            1000
        )
        .await
        .is_err()
    );
    usage.memories[0].purpose = MemoryUsePurpose::HistoricalReference;
    assert_eq!(
        validate_on(
            &store.db,
            "1",
            "current-device",
            &usage,
            &access,
            None,
            &destination("model"),
            &policy,
            1000
        )
        .await
        .unwrap(),
        Some(2000)
    );
    assert!(
        validate_on(
            &store.db,
            "1",
            "current-device",
            &usage,
            &access,
            None,
            &destination("model"),
            &policy,
            2000
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn owner_view_and_no_export_retention_never_become_model_read_permission() {
    let store = setup().await;
    let (memory, source, origin_id) = super::owner_memory::seed(&store, 1, "no-export").await;
    let original = origin::Entity::find()
        .filter(origin::Column::OriginId.eq(&origin_id))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let mut policy: SourceUsePolicy = serde_json::from_str(&original.policy_json).unwrap();
    policy.no_export = true;
    let encoded = serde_json::to_string(&policy).unwrap();
    origin::Entity::update_many()
        .col_expr(
            origin::Column::PolicyJson,
            sea_orm::sea_query::Expr::value(encoded.clone()),
        )
        .filter(origin::Column::OriginId.eq(origin_id))
        .exec(&store.db)
        .await
        .unwrap();
    evidence::Entity::update_many()
        .col_expr(
            evidence::Column::PolicyJson,
            sea_orm::sea_query::Expr::value(encoded),
        )
        .filter(evidence::Column::Id.eq(desk_assistant_data_store::source_rows::memory(&source)))
        .exec(&store.db)
        .await
        .unwrap();
    super::owner_memory::rebuild_evidence_proof(&store, &source).await;
    assert_eq!(
        store
            .owner_source(1, &source, &AssistantDataPolicy::default(), 1000)
            .await
            .unwrap()
            .availability,
        SourceAvailability::EvidenceOnly
    );
    let mut usage = memory_usage(&store, &memory, &source).await;
    let access = access(&store).await;
    assert!(
        validate_on(
            &store.db,
            "1",
            "current-device",
            &usage,
            &access,
            None,
            &destination("model"),
            &AssistantDataPolicy::default(),
            1000
        )
        .await
        .is_err()
    );
    usage.purpose = SourceReadPurpose::OwnerHistory;
    assert!(
        validate_on(
            &store.db,
            "1",
            "current-device",
            &usage,
            &access,
            None,
            &destination("model"),
            &AssistantDataPolicy::default(),
            1000
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn a_fresh_generation_cannot_recover_a_forgotten_fragment_through_history() {
    let store = setup().await;
    let (memory, source, _) = super::owner_memory::seed(&store, 1, "forgotten").await;
    let original = super::owner_memory::seed_original_history(&store, &source).await;
    let (device, before) = history_usage(&store, &original).await;
    let initial = access(&store).await;
    let scope = owner_history_scope(
        "1",
        "root",
        &device,
        10,
        100,
        "episode",
        1,
        &initial.settings,
        "UTC",
        false,
    )
    .unwrap()
    .unwrap();
    let platform = AssistantDataPolicy::default();
    validate_on(
        &store.db,
        "1",
        &device,
        &before,
        &initial,
        Some(&scope),
        &destination("model"),
        &platform,
        1000,
    )
    .await
    .unwrap();
    store
        .forget_memory(
            1,
            &MemoryDeleteRequest {
                memory_id: memory,
                expected_revision: 1,
                operation_id: "forget-source".into(),
            },
            &platform,
            1001,
        )
        .await
        .unwrap();
    let (device, after) = history_usage(&store, &original).await;
    let latest = access(&store).await;
    assert!(
        validate_on(
            &store.db,
            "1",
            &device,
            &after,
            &latest,
            Some(&scope),
            &destination("model"),
            &platform,
            1002
        )
        .await
        .is_err()
    );
    assert_eq!(
        store
            .owner_source(1, &original, &platform, 1002)
            .await
            .unwrap()
            .availability,
        SourceAvailability::Available
    );
}

#[tokio::test]
async fn missing_dependency_wrong_scope_and_feature_switches_fail_before_export() {
    let store = setup().await;
    let (memory, source, _) = super::owner_memory::seed(&store, 1, "scope").await;
    let original = super::owner_memory::seed_original_history(&store, &source).await;
    let mut usage = memory_usage(&store, &memory, &source).await;
    let mut access = access(&store).await;
    let platform = AssistantDataPolicy::default();
    usage.history_dependencies.clear();
    assert!(
        validate_on(
            &store.db,
            "1",
            "current-device",
            &usage,
            &access,
            None,
            &destination("model"),
            &platform,
            1000
        )
        .await
        .is_err()
    );
    let usage = memory_usage(&store, &memory, &source).await;
    access.settings.automatic_memory_enabled = false;
    assert!(
        validate_on(
            &store.db,
            "1",
            "current-device",
            &usage,
            &access,
            None,
            &destination("model"),
            &platform,
            1000
        )
        .await
        .is_err()
    );
    access.settings.automatic_memory_enabled = true;
    let (_, history) = history_usage(&store, &original).await;
    let mut scope = owner_history_scope(
        "1",
        "root",
        "another-device",
        10,
        100,
        "episode",
        1,
        &access.settings,
        "UTC",
        false,
    )
    .unwrap()
    .unwrap();
    scope.device_rule =
        desk_agent_protocol::ai_assistant::history::AssistantDeviceScope::CurrentDevice;
    assert!(
        validate_on(
            &store.db,
            "1",
            "another-device",
            &history,
            &access,
            Some(&scope),
            &destination("model"),
            &platform,
            1000
        )
        .await
        .is_err()
    );
    scope.device_rule =
        desk_agent_protocol::ai_assistant::history::AssistantDeviceScope::AllMyDevices;
    validate_on(
        &store.db,
        "1",
        "another-device",
        &history,
        &access,
        Some(&scope),
        &destination("model"),
        &platform,
        1000,
    )
    .await
    .unwrap();
    access.settings.model_history_search_enabled = false;
    assert!(
        validate_on(
            &store.db,
            "1",
            "another-device",
            &history,
            &access,
            Some(&scope),
            &destination("model"),
            &platform,
            1000
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn a_window_decision_anchor_is_validated_without_pretending_its_text_is_returned_or_learned()
{
    let store = setup().await;
    let (_, source, _) = super::owner_memory::seed(&store, 1, "window-decision").await;
    let original = super::owner_memory::seed_original_history(&store, &source).await;
    let (device, mut usage) = history_usage(&store, &original).await;
    usage.decision_roots = std::mem::take(&mut usage.roots);
    let access = access(&store).await;
    let platform = AssistantDataPolicy::default();
    let scope = owner_history_scope(
        "1",
        "root",
        &device,
        10,
        100,
        "episode",
        1,
        &access.settings,
        "UTC",
        false,
    )
    .unwrap()
    .unwrap();
    usage.validate().unwrap();
    validate_on(
        &store.db,
        "1",
        &device,
        &usage,
        &access,
        Some(&scope),
        &destination("model"),
        &platform,
        1000,
    )
    .await
    .unwrap();
    assert!(usage.roots.is_empty());
    let mut padded = usage.clone();
    let mut unrelated = padded.history_dependencies[0].clone();
    unrelated.source_id = native_ids::fixture_source_id(&"unrelated-decision-padding");
    padded.history_dependencies.push(unrelated);
    padded.validate().unwrap();
    assert!(
        validate_on(
            &store.db,
            "1",
            &device,
            &padded,
            &access,
            Some(&scope),
            &destination("model"),
            &platform,
            1000
        )
        .await
        .is_err()
    );
    let mut no_dependency = usage.clone();
    no_dependency.history_dependencies.clear();
    assert!(no_dependency.validate().is_err());
    let mut learning = usage.clone();
    learning.purpose = SourceReadPurpose::ModelMemory;
    assert!(learning.validate().is_err());
    let origin = &usage
        .history_dependencies
        .iter()
        .find(|source| source.source_id == original)
        .unwrap()
        .origin_id;
    origin::Entity::update_many()
        .col_expr(
            origin::Column::DeletedAtMs,
            sea_orm::sea_query::Expr::value(Some(1001_i64)),
        )
        .filter(origin::Column::OriginId.eq(origin))
        .exec(&store.db)
        .await
        .unwrap();
    assert!(
        validate_on(
            &store.db,
            "1",
            &device,
            &usage,
            &access,
            Some(&scope),
            &destination("model"),
            &platform,
            1002
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn a_separate_pending_memory_does_not_change_the_current_record_identified_by_id() {
    let store = setup().await;
    let (memory, source, _) = super::owner_memory::seed(&store, 1, "pending-slot-current").await;
    let (pending, _, _) = super::owner_memory::seed(&store, 1, "pending-slot-other").await;
    fact::Entity::update_many()
        .col_expr(
            fact::Column::Status,
            sea_orm::sea_query::Expr::value("pending_conflict"),
        )
        .filter(fact::Column::MemoryId.eq(&pending))
        .exec(&store.db)
        .await
        .unwrap();
    version::Entity::update_many()
        .col_expr(
            version::Column::Status,
            sea_orm::sea_query::Expr::value("pending_conflict"),
        )
        .filter(version::Column::MemoryId.eq(&pending))
        .exec(&store.db)
        .await
        .unwrap();
    let mut usage = memory_usage(&store, &memory, &source).await;
    let access = access(&store).await;
    let policy = AssistantDataPolicy::default();
    assert!(
        validate_on(
            &store.db,
            "1",
            "current-device",
            &usage,
            &access,
            None,
            &destination("model"),
            &policy,
            1000
        )
        .await
        .is_ok()
    );
    usage.memories[0].purpose = MemoryUsePurpose::HistoricalReference;
    validate_on(
        &store.db,
        "1",
        "current-device",
        &usage,
        &access,
        None,
        &destination("model"),
        &policy,
        1000,
    )
    .await
    .unwrap();
}
