use super::*;
mod group;
use crate::entity::{
    agent_assistant_device_binding as binding, agent_assistant_origin as origin,
    agent_memory as fact, agent_memory_revision as fact_revision, agent_memory_source as evidence,
    agent_memory_tombstone as tombstone,
};
use desk_agent_protocol::ai_assistant::{
    memory::{DataControlOutcome, MemoryClearRequest, MemoryDeleteRequest, MemoryListRequest},
    source::HistoryDependency,
};
use desk_diagnose_core::{
    ai_assistant::memory_control::KnownMemorySource,
    native_ids::{self, Kind},
};
use sea_orm::PaginatorTrait;

pub(super) async fn seed(store: &TestStore, actor: i32, label: &str) -> (String, String, String) {
    let body = format!("Prefer Chinese for {label}");
    seed_with_body(store, actor, label, &body).await
}

pub(super) async fn seed_with_body(
    store: &TestStore,
    actor: i32,
    label: &str,
    body: &str,
) -> (String, String, String) {
    store
        .settings(actor, &AssistantDataPolicy::default())
        .await
        .unwrap();
    let actor = actor.to_string();
    ensure_quota_on(&store.db, &actor).await.unwrap();
    let previous = runtime_on(&store.db, &actor).await.unwrap();
    let accepted_seq = increment(previous.input_seq).unwrap().max(2);
    let generation = increment(previous.memory_state_generation).unwrap();
    let config = store.db.config_read().await;
    let audience = format!("offline-device-{label}");
    let device_ref = native_ids::stable(
        Kind::AssistantDeviceBinding,
        &(&config.metadata.instance, &audience),
    );
    binding::ActiveModel {
        binding_id: Set(device_ref.clone()),
        instance_id: Set(config.metadata.instance.clone()),
        audience: Set(audience),
        device_name: Set(Some(label.into())),
        first_seen_at_ms: Set(1),
        last_seen_at_ms: Set(1),
        ..Default::default()
    }
    .insert(&store.db)
    .await
    .unwrap();
    let policy = desk_diagnose_core::ai_assistant::source::owner_text_policy(1, false);
    let origin_id = native_ids::stable(Kind::AssistantOrigin, &(actor.as_str(), label));
    origin::ActiveModel {
        origin_id: Set(origin_id.clone()),
        actor_id: Set(actor.clone()),
        device_ref: Set(device_ref.clone()),
        conversation_id: Set(format!("conversation-{label}")),
        root_conversation_id: Set(format!("conversation-{label}")),
        native_id: Set(native_ids::stable(Kind::Message, &label)),
        accepted_input_seq: Set(accepted_seq),
        kind: Set("input".into()),
        original_sha256: Set(format!("{:x}", Sha256::digest(b"I prefer Chinese"))),
        deletion_generation: Set(1),
        deleted_at_ms: Set(None),
        policy_json: Set(serde_json::to_string(&policy).unwrap()),
        created_at_ms: Set(1),
        statement_timezone: Set("UTC".into()),
        ..Default::default()
    }
    .insert(&store.db)
    .await
    .unwrap();
    let memory_id = native_ids::stable(Kind::Memory, &(actor.as_str(), label));
    let source_id = native_ids::stable(Kind::AssistantSource, &(label, "evidence"));
    let known = KnownMemorySource {
        source_id: native_ids::stable(Kind::AssistantSource, &(label, "original")),
        revision: 1,
        origin_id: origin_id.clone(),
        content_sha256: format!("{:x}", Sha256::digest(b"I prefer Chinese")),
    };
    let body = body.to_owned();
    fact::ActiveModel {
        memory_id: Set(memory_id.clone()),
        actor_id: Set(actor.clone()),
        scope_key: Set("personal".into()),
        device_ref: Set(String::new()),

        body: Set(body.clone()),
        context: Set(Some("Owner preference".into())),
        content_sha256: Set(memory::content_hash(&body, Some("Owner preference"))),
        revision: Set(1),
        state_generation: Set(generation),
        status: Set("active".into()),
        accepted_input_seq: Set(accepted_seq),
        observed_at_ms: Set(1),
        updated_at_ms: Set(1),
        valid_from_ms: Set(1),
        valid_until_ms: Set(None),
        ..Default::default()
    }
    .insert(&store.db)
    .await
    .unwrap();
    evidence::ActiveModel {
        source_id: Set(source_id.clone()),
        memory_id: Set(memory_id.clone()),
        memory_revision: Set(1),
        actor_id: Set(actor.clone()),
        origin_id: Set(origin_id.clone()),
        role: Set("evidence".into()),
        body: Set("I prefer Chinese".into()),
        content_sha256: Set(format!("{:x}", Sha256::digest(b"I prefer Chinese"))),
        retained_bytes: Set("I prefer Chinese".len() as i32),
        body_cleared_at_ms: Set(None),
        dependencies_json: Set("[]".into()),
        known_sources_json: Set(serde_json::to_string(&vec![known.clone()]).unwrap()),
        forget_sources_json: Set(serde_json::to_string(&vec![known]).unwrap()),
        retention_proof_json: Set("{}".into()),
        policy_json: Set(serde_json::to_string(&policy).unwrap()),
        source_device_json: Set(None),
        observed_at_ms: Set(1),
        statement_timezone: Set("UTC".into()),
        revoked_at_ms: Set(None),
        expires_at_ms: Set(None),
        ..Default::default()
    }
    .insert(&store.db)
    .await
    .unwrap();
    fact_revision::ActiveModel {
        memory_id: Set(memory_id.clone()),
        revision: Set(1),
        state_generation: Set(generation),
        actor_id: Set(actor.clone()),
        producer_call_id: Set(format!("call-{label}")),
        tool_call_id: Set(format!("tool-{label}")),
        request_sha256: Set("b".repeat(64)),
        logical_mutation_id: Set(format!("fixture-{label}")),
        write_outcome: Set("applied".into()),
        body: Set(body.clone()),
        context: Set(Some("Owner preference".into())),
        content_sha256: Set(memory::content_hash(&body, Some("Owner preference"))),
        status: Set("active".into()),
        accepted_input_seq: Set(accepted_seq),
        dependencies_json: Set("[]".into()),
        created_at_ms: Set(1),
        expires_at_ms: Set(None),
        ..Default::default()
    }
    .insert(&store.db)
    .await
    .unwrap();
    runtime::Entity::update_many()
        .col_expr(
            runtime::Column::InputSeq,
            sea_orm::sea_query::Expr::value(accepted_seq.max(10)),
        )
        .col_expr(
            runtime::Column::MemoryStateGeneration,
            sea_orm::sea_query::Expr::value(generation),
        )
        .col_expr(
            runtime::Column::Revision,
            sea_orm::sea_query::Expr::value(increment(previous.revision).unwrap()),
        )
        .filter(runtime::Column::ActorId.eq(actor))
        .exec(&store.db)
        .await
        .unwrap();
    rebuild_evidence_proof(store, &source_id).await;
    (memory_id, source_id, origin_id)
}

pub(super) async fn rebuild_evidence_proof(store: &TestStore, source_id: &str) {
    use desk_agent_protocol::ai_assistant::source::{
        AssistantSourceKind, SourceRefRole, SourceUsePolicy,
    };
    use desk_diagnose_core::ai_assistant::memory_evidence::{
        MemoryEvidenceRetentionProof, MemoryEvidenceSnapshot,
    };
    let source = evidence::Entity::find()
        .filter(evidence::Column::SourceId.eq(source_id))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let original = origin::Entity::find()
        .filter(origin::Column::OriginId.eq(&source.origin_id))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let version = fact_revision::Entity::find()
        .filter(fact_revision::Column::MemoryId.eq(&source.memory_id))
        .filter(fact_revision::Column::Revision.eq(source.memory_revision))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let origin_policy: SourceUsePolicy = serde_json::from_str(&original.policy_json).unwrap();
    let evidence_policy: SourceUsePolicy = serde_json::from_str(&source.policy_json).unwrap();
    let known: Vec<KnownMemorySource> = serde_json::from_str(&source.known_sources_json).unwrap();
    let forget: Vec<KnownMemorySource> = serde_json::from_str(&source.forget_sources_json).unwrap();
    let dependencies: Vec<HistoryDependency> =
        serde_json::from_str(&source.dependencies_json).unwrap();
    let proof = MemoryEvidenceRetentionProof::from_snapshot(&MemoryEvidenceSnapshot {
        retained_origins: &[],
        actor_id: &source.actor_id,
        memory_id: &source.memory_id,
        memory_revision: unsigned(source.memory_revision).unwrap(),
        memory_accepted_input_seq: unsigned(version.accepted_input_seq).unwrap(),
        evidence_source_id: &source.source_id,
        role: SourceRefRole::Evidence,
        body: &source.body,
        origin_id: &original.origin_id,
        origin_kind: AssistantSourceKind::Input,
        origin_deletion_generation: unsigned(original.deletion_generation).unwrap(),
        origin_accepted_input_seq: unsigned(original.accepted_input_seq).unwrap(),
        origin_content_sha256: &original.original_sha256,
        recorded_at_unix_ms: unsigned(source.observed_at_ms).unwrap(),
        statement_timezone: &source.statement_timezone,
        origin_policy: &origin_policy,
        evidence_policy: &evidence_policy,
        known_sources: &known,
        forget_sources: &forget,
        production_dependencies: &dependencies,
    })
    .unwrap();
    evidence::Entity::update_many()
        .col_expr(
            evidence::Column::RetentionProofJson,
            sea_orm::sea_query::Expr::value(serde_json::to_string(&proof).unwrap()),
        )
        .filter(evidence::Column::SourceId.eq(source_id))
        .exec(&store.db)
        .await
        .unwrap();
}

#[tokio::test]
async fn owner_management_is_available_when_file_settings_disable_model_memory_and_search() {
    let store = setup().await;
    let (memory_id, _, _) = seed(&store, 1, "A").await;
    let policy = AssistantDataPolicy::default();
    let current = store.settings(1, &policy).await.unwrap();
    store
        .update_settings(
            1,
            &AssistantDataSettingsUpdate {
                expected_revision: current.revision,
                automatic_memory_enabled: false,
                model_history_search_enabled: false,

                explanation_version: 1,
            },
            &policy,
        )
        .await
        .unwrap();
    let listed = store
        .memories(1, &MemoryListRequest::default(), &policy, 100 * 86_400_000)
        .await
        .unwrap();
    assert_eq!(listed.memories.len(), 1);
    assert_eq!(listed.memories[0].memory_id, memory_id);
    assert!(
        !crate::assistant_data_config::settings(&store.db)
            .await
            .automatic_memory_enabled
    );
    assert!(matches!(
        ensure_runtime_on(&store.db, "2").await,
        Err(AssistantStoreError::Inaccessible)
    ));
    let removed = store
        .forget_memory(
            1,
            &MemoryDeleteRequest {
                memory_id,
                expected_revision: 1,
                operation_id: "delete-while-disabled".into(),
            },
            &policy,
            100 * 86_400_000,
        )
        .await
        .unwrap();
    assert_eq!(removed.outcome, DataControlOutcome::Applied);
}

#[tokio::test]
async fn delete_revokes_evidence_and_replay_preserves_generations_and_original_history_origin() {
    let store = setup().await;
    let (memory_id, source_id, origin_id) = seed(&store, 1, "A").await;
    let policy = AssistantDataPolicy::default();
    let request = MemoryDeleteRequest {
        memory_id: memory_id.clone(),
        expected_revision: 1,
        operation_id: "delete-A".into(),
    };
    let applied = store
        .forget_memory(1, &request, &policy, 1000)
        .await
        .unwrap();
    assert_eq!(applied.outcome, DataControlOutcome::Applied);
    assert_eq!(applied.affected_count, 1);
    assert!(applied.cleanup_pending);
    let after = runtime_on(&store.db, "1").await.unwrap();
    assert_eq!(after.input_seq, 11);
    assert_eq!(after.learning_cutoff_input_seq, 0);
    assert!(
        store
            .memories(1, &MemoryListRequest::default(), &policy, 1000)
            .await
            .unwrap()
            .memories
            .is_empty()
    );
    let source = evidence::Entity::find()
        .filter(evidence::Column::SourceId.eq(source_id))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(source.revoked_at_ms, Some(1000));
    let original = origin::Entity::find()
        .filter(origin::Column::OriginId.eq(origin_id))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(original.deleted_at_ms, None);
    let replayed = store
        .forget_memory(1, &request, &policy, 2000)
        .await
        .unwrap();
    assert_eq!(replayed.outcome, DataControlOutcome::AlreadyApplied);
    assert_eq!(replayed.control_generation, applied.control_generation);
    assert_eq!(runtime_on(&store.db, "1").await.unwrap(), after);
    assert_eq!(tombstone::Entity::find().count(&store.db).await.unwrap(), 1);
    let mut changed = request;
    changed.expected_revision = 2;
    assert!(matches!(
        store.forget_memory(1, &changed, &policy, 3000).await,
        Err(AssistantStoreError::Invalid(_))
    ));
    assert_eq!(runtime_on(&store.db, "1").await.unwrap(), after);
}

#[tokio::test]
async fn stale_delete_and_clear_do_not_revoke_newer_facts_and_clear_replay_does_not_repeat() {
    let store = setup().await;
    let (memory_id, _, _) = seed(&store, 1, "A").await;
    let policy = AssistantDataPolicy::default();
    let state = runtime_on(&store.db, "1").await.unwrap();
    let stale = store
        .forget_memory(
            1,
            &MemoryDeleteRequest {
                memory_id,
                expected_revision: 99,
                operation_id: "stale".into(),
            },
            &policy,
            1000,
        )
        .await
        .unwrap();
    assert_eq!(stale.outcome, DataControlOutcome::StaleTarget);
    assert_eq!(runtime_on(&store.db, "1").await.unwrap(), state);
    let stale_clear = store
        .clear_memories(
            1,
            &MemoryClearRequest {
                operation_id: "clear-stale".into(),
                expected_memory_state_generation: 99,
            },
            &policy,
            1000,
        )
        .await
        .unwrap();
    assert_eq!(stale_clear.outcome, DataControlOutcome::StaleTarget);
    assert_eq!(runtime_on(&store.db, "1").await.unwrap(), state);
    let request = MemoryClearRequest {
        operation_id: "clear-A".into(),
        expected_memory_state_generation: unsigned(state.memory_state_generation).unwrap(),
    };
    let cleared = store
        .clear_memories(1, &request, &policy, 2000)
        .await
        .unwrap();
    assert_eq!(cleared.affected_count, 1);
    let after = runtime_on(&store.db, "1").await.unwrap();
    assert_eq!(after.learning_cutoff_input_seq, 11);
    seed(&store, 1, "new-assertion").await;
    let replay = store
        .clear_memories(1, &request, &policy, 3000)
        .await
        .unwrap();
    assert_eq!(replay.outcome, DataControlOutcome::AlreadyApplied);
    let remaining = store
        .memories(1, &MemoryListRequest::default(), &policy, 3000)
        .await
        .unwrap();
    assert_eq!(remaining.memories.len(), 1);
    assert!(remaining.memories[0].content.contains("new-assertion"));
}

#[tokio::test]
async fn device_or_full_producer_dependency_revocation_hides_a_fact_before_cleanup() {
    let store = setup().await;
    let (_, _, other_origin) = seed(&store, 1, "B").await;
    let (memory_id, source_id, origin_id) = seed(&store, 1, "A").await;
    let dependency = HistoryDependency {
        source_id: native_ids::stable(Kind::AssistantSource, &"producer"),
        source_revision: 1,
        content_sha256: "a".repeat(64),
        origin_id: other_origin.clone(),
        origin_deletion_generation: 1,
        policy_revision: 1,
        expires_at_unix_ms: None,
    };
    evidence::Entity::update_many()
        .col_expr(
            evidence::Column::DependenciesJson,
            sea_orm::sea_query::Expr::value(serde_json::to_string(&vec![dependency]).unwrap()),
        )
        .filter(evidence::Column::SourceId.eq(&source_id))
        .exec(&store.db)
        .await
        .unwrap();
    rebuild_evidence_proof(&store, &source_id).await;
    let before = store
        .memories(
            1,
            &MemoryListRequest::default(),
            &AssistantDataPolicy::default(),
            999,
        )
        .await
        .unwrap();
    assert!(before.memories.iter().any(|row| row.memory_id == memory_id));
    origin::Entity::update_many()
        .col_expr(
            origin::Column::DeletedAtMs,
            sea_orm::sea_query::Expr::value(1000),
        )
        .filter(origin::Column::OriginId.eq(other_origin))
        .exec(&store.db)
        .await
        .unwrap();
    let list = store
        .memories(
            1,
            &MemoryListRequest::default(),
            &AssistantDataPolicy::default(),
            1001,
        )
        .await
        .unwrap();
    assert!(list.memories.iter().all(|row| row.memory_id != memory_id));
    assert!(
        origin::Entity::find()
            .filter(origin::Column::OriginId.eq(origin_id))
            .one(&store.db)
            .await
            .unwrap()
            .unwrap()
            .deleted_at_ms
            .is_none()
    );
    // The owner can still remove inaccessible data using a previously seen identity.
    assert_eq!(
        store
            .forget_memory(
                1,
                &MemoryDeleteRequest {
                    memory_id,
                    expected_revision: 1,
                    operation_id: "remove-restricted".into()
                },
                &AssistantDataPolicy::default(),
                1002
            )
            .await
            .unwrap()
            .outcome,
        DataControlOutcome::Applied
    );
}

#[tokio::test]
async fn pagination_is_owner_scoped_and_rejects_a_changed_generation() {
    let store = setup().await;
    seed(&store, 1, "A").await;
    seed(&store, 1, "B").await;
    let policy = AssistantDataPolicy::default();
    let first = store
        .memories(
            1,
            &MemoryListRequest {
                limit: 1,
                ..Default::default()
            },
            &policy,
            100,
        )
        .await
        .unwrap();
    let request = MemoryListRequest {
        limit: 1,
        cursor: first.next_cursor.clone(),
        memory_state_generation: Some(first.memory_state_generation),
        control_generation: Some(first.control_generation),
        ..Default::default()
    };
    let second = store.memories(1, &request, &policy, 100).await.unwrap();
    assert_eq!(second.memories.len(), 1);
    assert_ne!(first.memories[0].memory_id, second.memories[0].memory_id);
    store
        .forget_memory(
            1,
            &MemoryDeleteRequest {
                memory_id: first.memories[0].memory_id.clone(),
                expected_revision: 1,
                operation_id: "change-list".into(),
            },
            &policy,
            200,
        )
        .await
        .unwrap();
    assert!(matches!(
        store.memories(1, &request, &policy, 300).await,
        Err(AssistantStoreError::Stale { .. })
    ));
}

pub(super) async fn seed_original_history(store: &TestStore, source_id: &str) -> String {
    use crate::entity::{
        agent_assistant_history as history, agent_assistant_search_document as document,
    };
    use desk_diagnose_core::ai_assistant::source::HistoryRetentionProof;
    let evidence = evidence::Entity::find()
        .filter(evidence::Column::SourceId.eq(source_id))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let known: Vec<KnownMemorySource> = serde_json::from_str(&evidence.known_sources_json).unwrap();
    let original = origin::Entity::find()
        .filter(origin::Column::OriginId.eq(&evidence.origin_id))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let policy: desk_agent_protocol::ai_assistant::source::SourceUsePolicy =
        serde_json::from_str(&original.policy_json).unwrap();
    origin::Entity::update_many()
        .col_expr(
            origin::Column::HistoryAdmitted,
            sea_orm::sea_query::Expr::value(true),
        )
        .filter(origin::Column::OriginId.eq(&original.origin_id))
        .exec(&store.db)
        .await
        .unwrap();
    let body = evidence.body;
    let hash = format!("{:x}", Sha256::digest(body.as_bytes()));
    let expires = 90 * 86_400_000i64;
    let proof =
        HistoryRetentionProof {
            native_metadata_sha256:
                desk_diagnose_core::ai_assistant::native_history::metadata_sha256(None).unwrap(),
            actor_id: original.actor_id.clone(),
            device_ref: original.device_ref.clone(),
            accepted_input_seq: unsigned(original.accepted_input_seq).unwrap(),
            origin_id: original.origin_id.clone(),
            original_envelope_id: "owner-input-envelope".into(),
            production_dependencies_sha256:
                desk_diagnose_core::ai_assistant::source::history_dependencies_sha256(&[]).unwrap(),
            original_content_sha256: original.original_sha256.clone(),
            retained_content_sha256: hash.clone(),
            source_policy_sha256: policy.policy_sha256,
            policy_revision: 1,
            recorded_at_unix_ms: 1,
            statement_timezone: "UTC".into(),
            expires_at_unix_ms: expires as u64,
        };
    history::ActiveModel {
        source_id: Set(known[0].source_id.clone()),
        origin_id: Set(original.origin_id.clone()),
        actor_id: Set(original.actor_id.clone()),
        device_ref: Set(original.device_ref.clone()),
        conversation_id: Set(original.conversation_id.clone()),
        root_conversation_id: Set(original.root_conversation_id.clone()),
        native_id: Set(original.native_id),
        fragment_index: Set(0),
        kind: Set("input".into()),
        role: Set("user".into()),
        content: Set(body.clone()),
        content_sha256: Set(hash.clone()),
        revision: Set(1),
        admitted_generation: Set(1),
        recorded_at_ms: Set(1),
        statement_timezone: Set("UTC".into()),
        native_action_at_ms: Set(None),
        native_action_from_ms: Set(None),
        native_action_until_ms: Set(None),
        time_basis: Set(None),
        state_json: Set(None),
        dependencies_json: Set("[]".into()),
        retention_proof_json: Set(serde_json::to_string(&proof).unwrap()),
        expires_at_ms: Set(expires),
        admission: Set("indexed".into()),
        deleted_at_ms: Set(None),
        created_at_ms: Set(1),
        updated_at_ms: Set(1),
        ..Default::default()
    }
    .insert(&store.db)
    .await
    .unwrap();
    document::ActiveModel {
        document_id: Set(native_ids::stable(
            Kind::AssistantSource,
            &(source_id, "index"),
        )),
        actor_id: Set(original.actor_id),
        device_ref: Set(original.device_ref),
        source_kind: Set("history".into()),
        source_id: Set(known[0].source_id.clone()),
        source_revision: Set(1),
        corpus_generation: Set(1),
        origin_id: Set(original.origin_id),
        root_conversation_id: Set(original.root_conversation_id),
        conversation_id: Set(original.conversation_id),
        fragment_index: Set(0),
        byte_start: Set(0),
        byte_end: Set(body.len() as i32),
        token_count: Set(3),
        policy_revision: Set(1),
        content_sha256: Set(hash),
        partial: Set(false),
        expires_at_ms: Set(Some(expires)),
        ..Default::default()
    }
    .insert(&store.db)
    .await
    .unwrap();
    known[0].source_id.clone()
}

#[tokio::test]
async fn forgetting_a_fact_restricts_its_evidence_but_owner_can_still_view_the_original_chat() {
    use desk_agent_protocol::ai_assistant::source::SourceAvailability;
    let store = setup().await;
    let (memory_id, source_id, _) = seed(&store, 1, "A").await;
    let original_id = seed_original_history(&store, &source_id).await;
    let policy = AssistantDataPolicy::default();
    let before = store
        .owner_source(1, &original_id, &policy, 1000)
        .await
        .unwrap();
    assert_eq!(before.availability, SourceAvailability::Available);
    assert_eq!(before.content.as_deref(), Some("I prefer Chinese"));
    assert!(before.locator.is_some());
    let linked = store
        .owner_source(1, &source_id, &policy, 1000)
        .await
        .unwrap();
    assert_eq!(linked.availability, SourceAvailability::Available);
    assert_eq!(linked.source_id, source_id);
    assert_eq!(linked.locator, before.locator);
    assert_eq!(
        super::super::owner_source::memory_source_on(&store.db, "1", &source_id, &policy, 1000)
            .await
            .unwrap()
            .availability,
        SourceAvailability::EvidenceOnly
    );

    store
        .forget_memory(
            1,
            &MemoryDeleteRequest {
                memory_id,
                expected_revision: 1,
                operation_id: "forget-keep-chat".into(),
            },
            &policy,
            1001,
        )
        .await
        .unwrap();
    let original = store
        .owner_source(1, &original_id, &policy, 1002)
        .await
        .unwrap();
    assert_eq!(original.availability, SourceAvailability::Available);
    assert_eq!(original.content, before.content);
    let evidence = store
        .owner_source(1, &source_id, &policy, 1002)
        .await
        .unwrap();
    assert_eq!(evidence.availability, SourceAvailability::Available);
    assert_eq!(evidence.content, before.content);
    assert_eq!(
        evidence.notice.as_deref(),
        Some("memory_forgotten_original_kept")
    );
    assert_eq!(
        super::super::owner_source::memory_source_on(&store.db, "1", &source_id, &policy, 1002)
            .await
            .unwrap()
            .availability,
        SourceAvailability::Unavailable
    );
    // Clearing the forgotten evidence body must not restore it, nor lose the
    // independently retained original chat's exact native navigation identity.
    evidence::Entity::update_many()
        .col_expr(evidence::Column::Body, sea_orm::sea_query::Expr::value(""))
        .col_expr(
            evidence::Column::BodyClearedAtMs,
            sea_orm::sea_query::Expr::value(Some(1002_i64)),
        )
        .filter(evidence::Column::SourceId.eq(&source_id))
        .exec(&store.db)
        .await
        .unwrap();
    let removed = evidence::Entity::find()
        .filter(evidence::Column::SourceId.eq(&source_id))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let original = store
        .owner_source(1, &source_id, &policy, 1003)
        .await
        .unwrap();
    assert_eq!(original.availability, SourceAvailability::Available);
    assert_eq!(original.locator, before.locator);
    assert_eq!(original.content, before.content);
    assert_eq!(
        evidence::Entity::find()
            .filter(evidence::Column::SourceId.eq(&source_id))
            .one(&store.db)
            .await
            .unwrap()
            .unwrap(),
        removed
    );
    assert!(
        store
            .memories(1, &MemoryListRequest::default(), &policy, 1003)
            .await
            .unwrap()
            .memories
            .is_empty()
    );
    assert_eq!(
        store
            .owner_source(1, &source_id, &policy, 100 * 86_400_000)
            .await
            .unwrap()
            .availability,
        SourceAvailability::Unavailable
    );
}

#[tokio::test]
async fn independent_fact_evidence_outlives_the_original_history_retention() {
    use desk_agent_protocol::ai_assistant::source::SourceAvailability;
    let store = setup().await;
    let (_, source_id, _) = seed(&store, 1, "A").await;
    let original_id = seed_original_history(&store, &source_id).await;
    let policy = AssistantDataPolicy::default();
    let now = 100 * 86_400_000;
    assert_eq!(
        store
            .owner_source(1, &original_id, &policy, now)
            .await
            .unwrap()
            .availability,
        SourceAvailability::Unavailable
    );
    let evidence = store
        .owner_source(1, &source_id, &policy, now)
        .await
        .unwrap();
    assert_eq!(evidence.availability, SourceAvailability::EvidenceOnly);
    assert_eq!(evidence.content.as_deref(), Some("I prefer Chinese"));
    assert_eq!(
        store
            .memories(1, &MemoryListRequest::default(), &policy, now)
            .await
            .unwrap()
            .memories
            .len(),
        1
    );
}

#[tokio::test]
async fn source_statement_timezone_does_not_follow_later_file_settings() {
    use desk_agent_protocol::ai_assistant::source::SourceAvailability;
    let store = setup().await;
    let (_, source_id, origin_id) = seed(&store, 1, "timezone").await;
    origin::Entity::update_many()
        .col_expr(
            origin::Column::StatementTimezone,
            sea_orm::sea_query::Expr::value("Asia/Shanghai"),
        )
        .filter(origin::Column::OriginId.eq(origin_id))
        .exec(&store.db)
        .await
        .unwrap();
    evidence::Entity::update_many()
        .col_expr(
            evidence::Column::StatementTimezone,
            sea_orm::sea_query::Expr::value("Asia/Shanghai"),
        )
        .filter(evidence::Column::SourceId.eq(source_id.as_str()))
        .exec(&store.db)
        .await
        .unwrap();
    rebuild_evidence_proof(&store, &source_id).await;
    let policy = AssistantDataPolicy::default();
    let settings = store.settings(1, &policy).await.unwrap();
    store
        .update_settings(
            1,
            &AssistantDataSettingsUpdate {
                expected_revision: settings.revision,
                automatic_memory_enabled: true,
                model_history_search_enabled: true,

                explanation_version: 1,
            },
            &policy,
        )
        .await
        .unwrap();
    let source = store
        .owner_source(1, &source_id, &policy, 1000)
        .await
        .unwrap();
    assert_eq!(source.availability, SourceAvailability::EvidenceOnly);
    assert_eq!(source.statement_timezone.as_deref(), Some("Asia/Shanghai"));
    evidence::Entity::update_many()
        .col_expr(
            evidence::Column::StatementTimezone,
            sea_orm::sea_query::Expr::value("Unknown/Timezone"),
        )
        .filter(evidence::Column::SourceId.eq(source_id.as_str()))
        .exec(&store.db)
        .await
        .unwrap();
    let unavailable = store
        .owner_source(1, &source_id, &policy, 1001)
        .await
        .unwrap();
    assert_eq!(unavailable.availability, SourceAvailability::Unavailable);
    assert!(unavailable.statement_timezone.is_none());
    assert!(unavailable.content.is_none());
}

#[tokio::test]
async fn invalid_retention_proof_hides_the_fact_without_blocking_owner_deletion() {
    use desk_agent_protocol::ai_assistant::source::SourceAvailability;
    let store = setup().await;
    let (memory_id, source_id, _) = seed(&store, 1, "invalid-proof").await;
    let policy = AssistantDataPolicy::default();
    assert_eq!(
        store
            .memories(1, &MemoryListRequest::default(), &policy, 1000)
            .await
            .unwrap()
            .memories
            .len(),
        1
    );
    evidence::Entity::update_many()
        .col_expr(
            evidence::Column::RetentionProofJson,
            sea_orm::sea_query::Expr::value("{}"),
        )
        .filter(evidence::Column::SourceId.eq(&source_id))
        .exec(&store.db)
        .await
        .unwrap();
    assert!(
        store
            .memories(1, &MemoryListRequest::default(), &policy, 1000)
            .await
            .unwrap()
            .memories
            .is_empty()
    );
    assert_eq!(
        store
            .owner_source(1, &source_id, &policy, 1000)
            .await
            .unwrap()
            .availability,
        SourceAvailability::Unavailable
    );
    assert_eq!(
        store
            .forget_memory(
                1,
                &MemoryDeleteRequest {
                    memory_id,
                    expected_revision: 1,
                    operation_id: "delete-invalid-proof".into()
                },
                &policy,
                1001
            )
            .await
            .unwrap()
            .outcome,
        DataControlOutcome::Applied
    );
}

#[tokio::test]
async fn updating_an_evidence_hash_cannot_rebind_the_retained_source_body_or_original_input() {
    let store = setup().await;
    let (_, source_id, origin_id) = seed(&store, 1, "original-binding").await;
    let policy = AssistantDataPolicy::default();
    evidence::Entity::update_many()
        .col_expr(
            evidence::Column::Body,
            sea_orm::sea_query::Expr::value("Invented source"),
        )
        .col_expr(
            evidence::Column::ContentSha256,
            sea_orm::sea_query::Expr::value(memory::content_hash_evidence("Invented source")),
        )
        .filter(evidence::Column::SourceId.eq(&source_id))
        .exec(&store.db)
        .await
        .unwrap();
    assert!(
        store
            .memories(1, &MemoryListRequest::default(), &policy, 1000)
            .await
            .unwrap()
            .memories
            .is_empty()
    );
    evidence::Entity::update_many()
        .col_expr(
            evidence::Column::Body,
            sea_orm::sea_query::Expr::value("I prefer Chinese"),
        )
        .col_expr(
            evidence::Column::ContentSha256,
            sea_orm::sea_query::Expr::value(memory::content_hash_evidence("I prefer Chinese")),
        )
        .filter(evidence::Column::SourceId.eq(&source_id))
        .exec(&store.db)
        .await
        .unwrap();
    assert_eq!(
        store
            .memories(1, &MemoryListRequest::default(), &policy, 1000)
            .await
            .unwrap()
            .memories
            .len(),
        1
    );
    origin::Entity::update_many()
        .col_expr(
            origin::Column::AcceptedInputSeq,
            sea_orm::sea_query::Expr::value(99_i64),
        )
        .filter(origin::Column::OriginId.eq(origin_id))
        .exec(&store.db)
        .await
        .unwrap();
    assert!(
        store
            .memories(1, &MemoryListRequest::default(), &policy, 1000)
            .await
            .unwrap()
            .memories
            .is_empty()
    );
}

#[tokio::test]
async fn current_fact_must_match_its_immutable_memory_revision() {
    let store = setup().await;
    let (memory_id, _, _) = seed(&store, 1, "revision-binding").await;
    let body = "Different body without a new revision";
    fact::Entity::update_many()
        .col_expr(fact::Column::Body, sea_orm::sea_query::Expr::value(body))
        .col_expr(
            fact::Column::ContentSha256,
            sea_orm::sea_query::Expr::value(memory::content_hash(body, Some("Owner preference"))),
        )
        .filter(fact::Column::MemoryId.eq(memory_id))
        .exec(&store.db)
        .await
        .unwrap();
    assert!(
        store
            .memories(
                1,
                &MemoryListRequest::default(),
                &AssistantDataPolicy::default(),
                1000
            )
            .await
            .unwrap()
            .memories
            .is_empty()
    );
}

// These fixtures add canonical metadata to already-retained source graphs. They
// do not represent a native extraction authorization or a production write.
#[tokio::test]
async fn owner_original_navigation_requires_the_exact_declared_version_hash_and_live_source() {
    use crate::entity::agent_assistant_history as history;
    use desk_agent_protocol::ai_assistant::source::SourceAvailability;
    for variant in 0..4 {
        let store = setup().await;
        let (_, source_id, origin_id) = seed(&store, 1, &format!("owner-source-{variant}")).await;
        let original = seed_original_history(&store, &source_id).await;
        match variant {
            0 => {
                history::Entity::update_many()
                    .col_expr(
                        history::Column::Revision,
                        sea_orm::sea_query::Expr::value(2_i64),
                    )
                    .filter(history::Column::SourceId.eq(&original))
                    .exec(&store.db)
                    .await
                    .unwrap();
            }
            1 => {
                history::Entity::update_many()
                    .col_expr(
                        history::Column::ContentSha256,
                        sea_orm::sea_query::Expr::value("f".repeat(64)),
                    )
                    .filter(history::Column::SourceId.eq(&original))
                    .exec(&store.db)
                    .await
                    .unwrap();
            }
            2 => {
                origin::Entity::update_many()
                    .col_expr(
                        origin::Column::DeletedAtMs,
                        sea_orm::sea_query::Expr::value(Some(999_i64)),
                    )
                    .filter(origin::Column::OriginId.eq(origin_id))
                    .exec(&store.db)
                    .await
                    .unwrap();
            }
            _ => {
                evidence::Entity::update_many()
                    .col_expr(
                        evidence::Column::ForgetSourcesJson,
                        sea_orm::sea_query::Expr::value("[]"),
                    )
                    .filter(evidence::Column::SourceId.eq(&source_id))
                    .exec(&store.db)
                    .await
                    .unwrap();
            }
        }
        let result = store
            .owner_source(1, &source_id, &AssistantDataPolicy::default(), 1000)
            .await
            .unwrap();
        assert!(result.locator.is_none());
        assert_eq!(
            result.availability,
            if variant < 2 {
                SourceAvailability::EvidenceOnly
            } else {
                SourceAvailability::Unavailable
            }
        );
    }
}

#[tokio::test]
async fn retained_memory_with_more_than_eight_sources_remains_readable_and_forgettable_after_lowering_limits()
 {
    use desk_agent_protocol::ai_assistant::memory::MemoryUsePurpose;
    use desk_diagnose_core::ai_assistant::data_control::CurrentMemoryUse;
    let store = setup().await;
    let (memory_id, source_id, _) = seed(&store, 1, "many-sources").await;
    let original = evidence::Entity::find()
        .filter(evidence::Column::SourceId.eq(&source_id))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    for index in 1..12 {
        let id = native_ids::stable(Kind::AssistantSource, &("many-sources", index));
        let mut active: evidence::ActiveModel = original.clone().into();
        active.id = sea_orm::ActiveValue::NotSet;
        active.source_id = Set(id.clone());
        active.insert(&store.db).await.unwrap();
        rebuild_evidence_proof(&store, &id).await;
    }
    let mut policy = AssistantDataPolicy::default();
    policy.limits.memory.source_refs = 1;
    let listed = store
        .memories(1, &MemoryListRequest::default(), &policy, 1000)
        .await
        .unwrap();
    assert_eq!(listed.memories.len(), 1);
    assert_eq!(listed.memories[0].source_ids.len(), 12);
    let reference = CurrentMemoryUse {
        memory_id: memory_id.clone(),
        revision: 1,
        purpose: MemoryUsePurpose::CurrentFact,
    };
    let sources =
        super::super::model_sources::memory_sources_on(&store.db, "1", "", &reference, 1000)
            .await
            .unwrap();
    assert_eq!(sources.0.len(), 12);
    let historical = CurrentMemoryUse {
        purpose: MemoryUsePurpose::HistoricalReference,
        ..reference
    };
    let retained =
        super::super::model_sources::memory_sources_on(&store.db, "1", "", &historical, 1000)
            .await
            .unwrap();
    assert_eq!(retained.0.len(), 12);
    assert!(
        super::super::model_sources::memory_sources_on(&store.db, "2", "", &historical, 1000)
            .await
            .is_err()
    );
    let request = MemoryDeleteRequest {
        memory_id: memory_id.clone(),
        expected_revision: 1,
        operation_id: "forget-many-sources".into(),
    };
    let result = store
        .forget_memory(1, &request, &policy, 1000)
        .await
        .unwrap();
    assert_eq!(result.outcome, DataControlOutcome::Applied);
    assert_eq!(
        evidence::Entity::find()
            .filter(evidence::Column::MemoryId.eq(&memory_id))
            .filter(evidence::Column::RevokedAtMs.eq(1000))
            .count(&store.db)
            .await
            .unwrap(),
        12
    );
    assert_eq!(
        store
            .forget_memory(1, &request, &policy, 1001)
            .await
            .unwrap()
            .outcome,
        DataControlOutcome::AlreadyApplied
    );
}
