use super::super::{
    memory_search as search,
    memory_view::{self, MemoryReadAuthorization},
    retrieval_cache,
};
use super::retrieval_native::Frame;
use super::*;
use crate::entity::{
    agent_assistant_data_authorization as authorization, agent_assistant_origin as origin,
    agent_assistant_read_projection as projection_row,
    agent_assistant_retrieval_query as query_row, agent_assistant_search_document as document,
    agent_assistant_search_term as term, agent_memory as fact, agent_memory_source as evidence,
};
use desk_agent_protocol::ai_assistant::{
    history::AssistantDeviceScope,
    memory::{MemoryListRequest, MemorySearchRequest, MemoryUsePurpose},
    source::{SourceReadPurpose, SourceUsePolicy},
};
use desk_diagnose_core::{
    ai_assistant::{
        data_access::{AssistantDataToolAccess, assistant_scope},
        data_tools,
        history::serialized_tool_response_bytes,
        retrieval::index_source,
        retrieval_query::QuerySourceVersion,
    },
    chat::ToolCall,
    native_ids::{self, Kind},
};
use sea_orm::{PaginatorTrait, sea_query::Expr};
use std::collections::BTreeSet;

fn request(frame: &Frame) -> MemorySearchRequest {
    serde_json::from_str(&frame.context().tool_call.arguments_json).unwrap()
}

async fn frame(label: &str, accepted: bool) -> Frame {
    Frame::configured(label, accepted, |_| {
        vec![ToolCall {
            id: "memory-call".into(),
            name: data_tools::SEARCH_MEMORIES.into(),
            arguments_json: "{\"queries\":[\"Chinese\"]}".into(),
        }]
    })
    .await
}

async fn current_fact(frame: &Frame) -> fact::Model {
    let reference = &frame
        .session
        .assistant_data
        .as_ref()
        .unwrap()
        .current_memories[0];
    fact::Entity::find()
        .filter(fact::Column::MemoryId.eq(&reference.memory_id))
        .one(&frame.store.db)
        .await
        .unwrap()
        .unwrap()
}

// This adds the sparse index for the already seeded, independently retained
// fixture fact. Actual native dispatch/acceptance still belongs to Frame.
async fn index_current(frame: &Frame) -> fact::Model {
    let row = current_fact(frame).await;
    index_fact(&frame.store, &row).await;
    row
}

pub(super) async fn index_fact(store: &TestStore, row: &fact::Model) {
    let source = evidence::Entity::find()
        .filter(evidence::Column::MemoryId.eq(&row.memory_id))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let projected = index_source(&format!(
        "{}\n{}",
        row.body,
        row.context.as_deref().unwrap_or("")
    ))
    .unwrap();
    for part in projected.fragments {
        let id = native_ids::fixture_source_id(&(
            "fixture-memory-index",
            &row.memory_id,
            part.fragment_index,
        ));
        document::ActiveModel {
            document_id: Set(id.clone()),
            actor_id: Set(row.actor_id.clone()),
            source_kind: Set("memory".into()),
            source_id: Set(row.memory_id.clone()),
            source_revision: Set(row.revision),
            device_ref: Set(row.device_ref.clone()),
            corpus_generation: Set(row.state_generation),
            origin_id: Set(source.origin_id.clone()),
            root_conversation_id: Set("fixture-root".into()),
            conversation_id: Set("fixture-root".into()),
            fragment_index: Set(part.fragment_index as i32),
            byte_start: Set(part.byte_start as i32),
            byte_end: Set(part.byte_end as i32),
            token_count: Set(part.token_count as i32),
            policy_revision: Set(1),
            partial: Set(projected.partial),
            expires_at_ms: Set(None),
            ..Default::default()
        }
        .insert(&store.db)
        .await
        .unwrap();
        for (text, count) in part.terms {
            term::ActiveModel {
                actor_id: Set(row.actor_id.clone()),
                document_id: Set(id.clone()),
                term: Set(text),
                frequency: Set(count as i32),
                source_revision: Set(row.revision),
                ..Default::default()
            }
            .insert(&store.db)
            .await
            .unwrap();
        }
    }
}

#[tokio::test]
async fn actual_accepted_producer_reads_model_memory_and_replays_the_identical_result_without_another_charge_or_send_grant()
 {
    let frame = frame("memory-search-valid", true).await;
    let row = index_current(&frame).await;
    let host = crate::agent_session_store::SignalAgentSessionStore::new(frame.store.db.clone())
        .with_client_metadata(
            None,
            desk_diagnose_core::session::AgentSessionSurface::AiAssistant,
        );
    let port = desk_diagnose_core::seam::SessionSeam::memory(&host).unwrap();
    let before = quota::Entity::find()
        .one(&frame.store.db)
        .await
        .unwrap()
        .unwrap();
    let grants = authorization::Entity::find()
        .count(&frame.store.db)
        .await
        .unwrap();
    let first = port
        .search_memories(frame.context(), &request(&frame))
        .await
        .unwrap();
    assert_eq!(first.result.memories.len(), 1);
    assert_eq!(first.result.memories[0].memory_id, row.memory_id);
    assert_eq!(first.result.memories[0].content, row.body);
    assert_eq!(first.result.memories[0].context, row.context);
    let proof = first.projection.as_ref().unwrap();
    assert_eq!(proof.usage.purpose, SourceReadPurpose::ModelMemory);
    assert_eq!(proof.usage.memories.len(), 1);
    assert_eq!(
        proof.usage.memories[0].purpose,
        MemoryUsePurpose::HistoricalReference
    );
    assert_eq!(
        proof
            .usage
            .roots
            .iter()
            .map(|root| root.source_id.clone())
            .collect::<BTreeSet<_>>(),
        first.result.memories[0]
            .source_ids
            .iter()
            .cloned()
            .collect()
    );
    let content = serde_json::to_string(&first.result).unwrap();
    proof.validate_cached_result(&content).unwrap();
    assert!(
        serialized_tool_response_bytes(&frame.context().tool_call.id, &content, None).unwrap()
            <= frame.context().maximum_response_bytes
    );
    let charged = quota::Entity::find()
        .one(&frame.store.db)
        .await
        .unwrap()
        .unwrap();
    assert!(charged.metadata_bytes > before.metadata_bytes);
    let replay = port
        .search_memories(frame.context(), &request(&frame))
        .await
        .unwrap();
    assert_eq!(first.result, replay.result);
    assert_eq!(first.projection, replay.projection);
    assert_eq!(
        quota::Entity::find()
            .one(&frame.store.db)
            .await
            .unwrap()
            .unwrap(),
        charged
    );
    assert_eq!(
        authorization::Entity::find()
            .count(&frame.store.db)
            .await
            .unwrap(),
        grants
    );
    let query = retrieval_cache::decode_row(
        query_row::Entity::find()
            .one(&frame.store.db)
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        query.proof.seen_sources,
        vec![QuerySourceVersion {
            source_id: row.memory_id,
            revision: row.revision as u64,
        }]
    );
}

#[tokio::test]
async fn keyword_search_does_not_fall_back_to_an_owner_list_when_its_index_has_no_hit() {
    let frame = frame("memory-search-no-index", true).await;
    let result = search::search(&frame.store.db, frame.context(), &request(&frame))
        .await
        .unwrap();
    assert!(result.result.memories.is_empty());
    assert!(!result.result.has_more);
    let proof = result.projection.unwrap();
    assert!(proof.usage.roots.is_empty());
    assert!(proof.usage.memories.is_empty());
    let listed = frame
        .store
        .memories(
            1,
            &MemoryListRequest::default(),
            &AssistantDataPolicy::default(),
            frame.now,
        )
        .await
        .unwrap();
    assert_eq!(listed.memories.len(), 1);
}

#[tokio::test]
async fn matching_foreign_account_facts_and_postings_never_enter_the_actual_model_search() {
    let frame = frame("memory-search-owner-A", true).await;
    let own = index_current(&frame).await;
    // Signal has one account. Inject foreign rows to test isolation without
    // creating a second account through an API that correctly rejects it.
    let foreign_id = native_ids::stable(Kind::Memory, &"memory-search-owner-B");
    let mut foreign: fact::ActiveModel = own.clone().into();
    foreign.id = sea_orm::ActiveValue::NotSet;
    foreign.memory_id = Set(foreign_id.clone());
    foreign.actor_id = Set("2".into());
    let foreign = foreign.insert(&frame.store.db).await.unwrap();
    let source = evidence::Entity::find()
        .filter(evidence::Column::MemoryId.eq(&own.memory_id))
        .one(&frame.store.db)
        .await
        .unwrap()
        .unwrap();
    let mut source: evidence::ActiveModel = source.into();
    source.id = Set(desk_assistant_data_store::source_rows::memory(
        native_ids::fixture_memory_source_id(&foreign_id),
    ));
    source.memory_id = Set(foreign_id);
    source.actor_id = Set("2".into());
    source.insert(&frame.store.db).await.unwrap();
    index_fact(&frame.store, &foreign).await;
    assert!(
        term::Entity::find()
            .filter(term::Column::ActorId.eq("2"))
            .count(&frame.store.db)
            .await
            .unwrap()
            > 0
    );

    let result = search::search(&frame.store.db, frame.context(), &request(&frame))
        .await
        .unwrap();
    assert_eq!(result.result.memories.len(), 1);
    assert_eq!(result.result.memories[0].memory_id, own.memory_id);
    assert!(
        !serde_json::to_string(&result.result)
            .unwrap()
            .contains(&foreign.memory_id)
    );
    assert!(matches!(
        frame
            .store
            .memories(
                2,
                &MemoryListRequest::default(),
                &AssistantDataPolicy::default(),
                frame.now
            )
            .await,
        Err(AssistantStoreError::Inaccessible)
    ));
}

#[tokio::test]
async fn an_unaccepted_native_producer_or_changed_tool_arguments_cannot_query_owner_memory() {
    let frame = frame("memory-search-unaccepted", false).await;
    index_current(&frame).await;
    let before = quota::Entity::find()
        .one(&frame.store.db)
        .await
        .unwrap()
        .unwrap();
    assert!(
        search::search(&frame.store.db, frame.context(), &request(&frame))
            .await
            .is_err()
    );
    assert_eq!(
        query_row::Entity::find()
            .count(&frame.store.db)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        quota::Entity::find()
            .one(&frame.store.db)
            .await
            .unwrap()
            .unwrap(),
        before
    );
    let frame = self::frame("memory-search-arguments", true).await;
    index_current(&frame).await;
    let mut changed = request(&frame);
    changed.limit += 1;
    frame
        .change_persisted_arguments(&serde_json::to_string(&changed).unwrap())
        .await;
    assert!(
        search::search(&frame.store.db, frame.context(), &request(&frame))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn final_query_cache_failure_rolls_back_memory_response_seen_ledger_and_quota() {
    let frame = frame("memory-search-rollback", true).await;
    index_current(&frame).await;
    let before = quota::Entity::find()
        .one(&frame.store.db)
        .await
        .unwrap()
        .unwrap();
    let projections = projection_row::Entity::find()
        .count(&frame.store.db)
        .await
        .unwrap();
    frame.store.db.execute_unprepared("CREATE TRIGGER reject_memory_query BEFORE INSERT ON agent_assistant_retrieval_query BEGIN SELECT RAISE(ABORT, 'final memory cache failure'); END").await.unwrap();
    assert!(
        search::search(&frame.store.db, frame.context(), &request(&frame))
            .await
            .is_err()
    );
    assert_eq!(
        query_row::Entity::find()
            .count(&frame.store.db)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        projection_row::Entity::find()
            .count(&frame.store.db)
            .await
            .unwrap(),
        projections
    );
    assert_eq!(
        quota::Entity::find()
            .one(&frame.store.db)
            .await
            .unwrap()
            .unwrap(),
        before
    );
}

#[tokio::test]
async fn disabling_model_memory_preserves_owner_management_and_blocks_cached_model_reads() {
    let frame = frame("memory-search-disabled", true).await;
    index_current(&frame).await;
    search::search(&frame.store.db, frame.context(), &request(&frame))
        .await
        .unwrap();
    let policy = AssistantDataPolicy::default();
    let settings = frame.store.settings(1, &policy).await.unwrap();
    frame
        .store
        .update_settings(
            1,
            &desk_agent_protocol::ai_assistant::user_instructions::AssistantDataSettingsUpdate {
                expected_revision: settings.revision,
                automatic_memory_enabled: false,
                model_history_search_enabled: settings.model_history_search_enabled,

                explanation_version: settings.explanation_version,
            },
            &policy,
        )
        .await
        .unwrap();
    assert!(
        search::search(&frame.store.db, frame.context(), &request(&frame))
            .await
            .is_err()
    );
    let listed = frame
        .store
        .memories(
            1,
            &MemoryListRequest::default(),
            &AssistantDataPolicy::default(),
            frame.now,
        )
        .await
        .unwrap();
    assert_eq!(listed.memories.len(), 1);
}

#[tokio::test]
async fn source_export_policy_changes_block_a_cached_memory_without_fabricating_another_native_call()
 {
    let frame = frame("memory-search-source-policy", true).await;
    index_current(&frame).await;
    let result = search::search(&frame.store.db, frame.context(), &request(&frame))
        .await
        .unwrap();
    let source = &result
        .projection
        .as_ref()
        .unwrap()
        .usage
        .history_dependencies[0];
    let row = origin::Entity::find()
        .filter(origin::Column::OriginId.eq(&source.origin_id))
        .one(&frame.store.db)
        .await
        .unwrap()
        .unwrap();
    let mut policy: SourceUsePolicy = serde_json::from_str(&row.policy_json).unwrap();
    policy.no_export = true;
    origin::Entity::update_many()
        .col_expr(
            origin::Column::PolicyJson,
            Expr::value(serde_json::to_string(&policy).unwrap()),
        )
        .filter(origin::Column::OriginId.eq(&row.origin_id))
        .exec(&frame.store.db)
        .await
        .unwrap();
    let before = quota::Entity::find()
        .one(&frame.store.db)
        .await
        .unwrap()
        .unwrap();
    assert!(
        search::search(&frame.store.db, frame.context(), &request(&frame))
            .await
            .is_err()
    );
    assert_eq!(
        quota::Entity::find()
            .one(&frame.store.db)
            .await
            .unwrap()
            .unwrap(),
        before
    );
}

#[tokio::test]
async fn the_original_observation_and_memory_device_rule_are_enforced_before_body_formatting() {
    let frame = frame("memory-view-observation", true).await;
    let row = current_fact(&frame).await;
    let data = frame.session.assistant_data.as_ref().unwrap();
    let scope = data.memory_access_scope.as_deref().unwrap();
    let policy = AssistantDataPolicy::default();
    let access = AssistantDataToolAccess {
        settings: settings_on(&frame.store.db, "1").await.unwrap(),
        policy_revision: policy.revision,
        control_generation: data.control_generation,
        memory_state_generation: data.memory_state_generation,
    };
    let devices = BTreeSet::from([data.source_device_ref.clone()]);
    let admitted = assistant_scope(&frame.session).unwrap();
    let version = QuerySourceVersion {
        source_id: row.memory_id,
        revision: row.revision as u64,
    };
    let mut authority = MemoryReadAuthorization {
        actor_id: "1",
        current_device_ref: &data.source_device_ref,
        assistant_scope: &admitted,
        scope,
        access: &access,
        platform: &policy,
        destination: frame.context().destination,
        requested_devices: &devices,
        observation_generation: row.state_generation as u64,
        now_unix_ms: frame.now,
    };
    let limited = desk_assistant_data_store::memory_keyword::scan_on::<
        _,
        super::super::memory_view::NativeMemoryReader,
    >(
        &frame.store.db,
        &authority,
        &request(&frame),
        std::time::Duration::ZERO,
    )
    .await
    .unwrap();
    assert!(limited.versions.is_empty());
    assert!(
        limited
            .reasons
            .iter()
            .any(|reason| reason == "search_time_limit")
    );
    assert!(
        memory_view::read_on(
            &frame.store.db,
            &authority,
            &version,
            MemoryUsePurpose::HistoricalReference
        )
        .await
        .is_ok()
    );
    authority.observation_generation -= 1;
    assert!(
        memory_view::read_on(
            &frame.store.db,
            &authority,
            &version,
            MemoryUsePurpose::HistoricalReference
        )
        .await
        .is_err()
    );
    let mut narrow = scope.clone();
    narrow.device_rule = AssistantDeviceScope::CurrentDevice;
    authority.scope = &narrow;
    authority.observation_generation = row.state_generation as u64;
    // Account facts remain allowed under a current-device scope. This does not
    // turn the source device into an execution target or relax source policy.
    assert!(
        memory_view::read_on(
            &frame.store.db,
            &authority,
            &version,
            MemoryUsePurpose::HistoricalReference
        )
        .await
        .is_ok()
    );
}

#[tokio::test]
async fn storage_views_apply_original_device_consent_and_keep_pending_facts_out_of_current_fact_use()
 {
    // These source graphs exercise the view gate. Real original native model
    // and persisted-call admission are covered by the Frame-based cases above.
    let store = setup().await;
    let (a, _, origin_a) = super::owner_memory::seed(&store, 1, "memory-view-A").await;
    let (b, _, origin_b) = super::owner_memory::seed(&store, 1, "memory-view-B").await;
    let original_a = origin::Entity::find()
        .filter(origin::Column::OriginId.eq(origin_a))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let original_b = origin::Entity::find()
        .filter(origin::Column::OriginId.eq(origin_b))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    fact::Entity::update_many()
        .col_expr(
            fact::Column::DeviceRef,
            Expr::value(original_b.device_ref.clone()),
        )
        .col_expr(
            fact::Column::ScopeKey,
            Expr::value(format!("device:{}", original_b.device_ref)),
        )
        .filter(fact::Column::MemoryId.eq(&b))
        .exec(&store.db)
        .await
        .unwrap();
    let row = fact::Entity::find()
        .filter(fact::Column::MemoryId.eq(&b))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let current = runtime_on(&store.db, "1").await.unwrap();
    let settings = settings_on(&store.db, "1").await.unwrap();
    let policy = AssistantDataPolicy::default();
    let access = AssistantDataToolAccess {
        settings: settings.clone(),
        policy_revision: policy.revision,
        control_generation: current.control_generation as u64,
        memory_state_generation: current.memory_state_generation as u64,
    };
    let scope = desk_diagnose_core::ai_assistant::memory_access::owner_memory_scope(
        "1",
        "view-root",
        &original_a.device_ref,
        current.input_seq as u64,
        1000,
        "view-episode",
        policy.revision,
        &settings,
        false,
    )
    .unwrap()
    .unwrap();
    let all_devices =
        BTreeSet::from([original_a.device_ref.clone(), original_b.device_ref.clone()]);
    let current_device = BTreeSet::from([original_a.device_ref.clone()]);
    let admitted = desk_diagnose_core::ai_assistant::data_access::AssistantScope::default();
    let destination = super::model_sources::destination("model");
    let version = QuerySourceVersion {
        source_id: row.memory_id.clone(),
        revision: row.revision as u64,
    };
    let mut authority = MemoryReadAuthorization {
        actor_id: "1",
        current_device_ref: &original_a.device_ref,
        assistant_scope: &admitted,
        scope: &scope,
        access: &access,
        platform: &policy,
        destination: &destination,
        requested_devices: &current_device,
        observation_generation: current.memory_state_generation as u64,
        now_unix_ms: 2000,
    };
    assert!(
        memory_view::read_on(
            &store.db,
            &authority,
            &version,
            MemoryUsePurpose::HistoricalReference
        )
        .await
        .is_err()
    );
    authority.requested_devices = &all_devices;
    assert!(
        memory_view::read_on(
            &store.db,
            &authority,
            &version,
            MemoryUsePurpose::HistoricalReference
        )
        .await
        .is_ok()
    );
    let mut narrow = scope.as_ref().clone();
    narrow.device_rule = AssistantDeviceScope::CurrentDevice;
    authority.scope = &narrow;
    assert!(
        memory_view::read_on(
            &store.db,
            &authority,
            &version,
            MemoryUsePurpose::HistoricalReference
        )
        .await
        .is_err()
    );
    authority.scope = &scope;
    let account = fact::Entity::find()
        .filter(fact::Column::MemoryId.eq(&a))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let account_version = QuerySourceVersion {
        source_id: a.clone(),
        revision: account.revision as u64,
    };
    fact::Entity::update_many()
        .col_expr(fact::Column::Status, Expr::value("pending_conflict"))
        .filter(fact::Column::MemoryId.eq(&a))
        .exec(&store.db)
        .await
        .unwrap();
    crate::entity::agent_memory_revision::Entity::update_many()
        .col_expr(
            crate::entity::agent_memory_revision::Column::Status,
            Expr::value("pending_conflict"),
        )
        .filter(crate::entity::agent_memory_revision::Column::MemoryId.eq(&a))
        .exec(&store.db)
        .await
        .unwrap();
    let uncertain = memory_view::read_on(
        &store.db,
        &authority,
        &account_version,
        MemoryUsePurpose::HistoricalReference,
    )
    .await
    .unwrap();
    assert_eq!(
        uncertain.record.status,
        desk_agent_protocol::ai_assistant::memory::MemoryStatus::PendingConflict
    );
    assert!(
        memory_view::read_on(
            &store.db,
            &authority,
            &account_version,
            MemoryUsePurpose::CurrentFact
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn one_thousand_independent_facts_keep_sparse_retrieval_bounded_and_readable() {
    // Corpus topics label synthetic stable preferences, not remembered actions.
    // Fixed keywords test retrieval separately from a live model's word choice.
    #[derive(serde::Deserialize)]
    struct Case {
        root: String,
        text: String,
        queries: Vec<String>,
        low_overlap: bool,
        bilingual: bool,
    }
    let cases: Vec<Case> = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../diagnose-core/tests/fixtures/assistant-retrieval-cases.json"
    )))
    .unwrap();
    assert_eq!(cases.len(), 60);
    assert_eq!(cases.iter().filter(|case| case.low_overlap).count(), 24);
    assert_eq!(cases.iter().filter(|case| case.bilingual).count(), 12);
    let mut topic_roots = BTreeSet::new();
    let topics: Vec<_> = cases
        .iter()
        .filter(|case| topic_roots.insert(case.root.clone()))
        .collect();
    let mut gold_sources = std::collections::BTreeMap::new();
    let store = setup().await;
    let mut devices = BTreeSet::new();
    let mut expected = String::new();
    for index in 0..1000 {
        let label = format!("capacity{index:04}");
        let (id, _, origin_id) = match topics.get(index) {
            Some(case) => {
                super::owner_memory::seed_with_body(
                    &store,
                    1,
                    &label,
                    &format!("Prefer Chinese for {}", case.text),
                )
                .await
            }
            None => super::owner_memory::seed(&store, 1, &label).await,
        };
        let original = origin::Entity::find()
            .filter(origin::Column::OriginId.eq(origin_id))
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        devices.insert(original.device_ref);
        let row = fact::Entity::find()
            .filter(fact::Column::MemoryId.eq(&id))
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        index_fact(&store, &row).await;
        if let Some(case) = topics.get(index) {
            gold_sources.insert(case.root.clone(), id.clone());
        }
        if index == 731 {
            expected = id;
        }
    }
    assert_eq!(fact::Entity::find().count(&store.db).await.unwrap(), 1000);
    let current = runtime_on(&store.db, "1").await.unwrap();
    let settings = settings_on(&store.db, "1").await.unwrap();
    let policy = AssistantDataPolicy::default();
    let access = AssistantDataToolAccess {
        settings: settings.clone(),
        policy_revision: policy.revision,
        control_generation: current.control_generation as u64,
        memory_state_generation: current.memory_state_generation as u64,
    };
    let device = devices.first().unwrap();
    let clock = chrono::Utc::now().timestamp_millis() as u64;
    let scope = desk_diagnose_core::ai_assistant::memory_access::owner_memory_scope(
        "1",
        "capacity-root",
        device,
        current.input_seq as u64,
        clock,
        "capacity-episode",
        policy.revision,
        &settings,
        false,
    )
    .unwrap()
    .unwrap();
    let admitted = desk_diagnose_core::ai_assistant::data_access::AssistantScope::default();
    let destination = super::model_sources::destination("model");
    let authority = MemoryReadAuthorization {
        actor_id: "1",
        current_device_ref: device,
        assistant_scope: &admitted,
        scope: &scope,
        access: &access,
        platform: &policy,
        destination: &destination,
        requested_devices: &devices,
        observation_generation: current.memory_state_generation as u64,
        now_unix_ms: clock,
    };
    let query = MemorySearchRequest {
        queries: vec!["capacity0731".into()],
        limit: 5,

        cursor: None,
        scope: desk_agent_protocol::ai_assistant::history::AssistantDeviceScope::AllMyDevices,
    };
    let before = crate::entity::agent_assistant_quota::Entity::find()
        .one(&store.db)
        .await
        .unwrap();
    let started = std::time::Instant::now();
    let result = super::super::memory_keyword::relevance_on(&store.db, &authority, &query)
        .await
        .unwrap();
    let elapsed = started.elapsed();
    assert_eq!(result.versions.len(), 1);
    assert_eq!(result.versions[0].source_id, expected);
    let readable = memory_view::read_on(
        &store.db,
        &authority,
        &result.versions[0],
        MemoryUsePurpose::CurrentFact,
    )
    .await
    .unwrap();
    assert_eq!(readable.row.memory_id, expected);
    assert!(
        elapsed < std::time::Duration::from_millis(500),
        "{elapsed:?}"
    );
    let mut timings = Vec::new();
    let mut reciprocal_rank = 0.0;
    for case in &cases {
        let query = MemorySearchRequest {
            queries: case.queries.clone(),
            limit: 5,

            cursor: None,
            scope: AssistantDeviceScope::AllMyDevices,
        };
        let started = std::time::Instant::now();
        let result = super::super::memory_keyword::relevance_on(&store.db, &authority, &query)
            .await
            .unwrap();
        timings.push(started.elapsed().as_micros());
        assert!(result.versions.len() <= 5);
        let expected = &gold_sources[&case.root];
        let rank = result
            .versions
            .iter()
            .position(|version| &version.source_id == expected)
            .unwrap_or_else(|| panic!("missing memory topic {}", case.root));
        reciprocal_rank += 1.0 / (rank + 1) as f64;
        let readable = memory_view::read_on(
            &store.db,
            &authority,
            &result.versions[rank],
            MemoryUsePurpose::CurrentFact,
        )
        .await
        .unwrap();
        assert_eq!(&readable.row.memory_id, expected);
    }
    timings.sort_unstable();
    assert!(timings[timings.len() * 95 / 100] < 500_000);
    assert!(reciprocal_rank / cases.len() as f64 >= 0.9);
    eprintln!(
        "assistant_memory_gold facts=1000 gold_queries=60 low_overlap_prompts=24 bilingual=12 recall_at_5=60/60 mrr={} p95_us={} max_us={} model_calls=0",
        reciprocal_rank / cases.len() as f64,
        timings[timings.len() * 95 / 100],
        timings.last().unwrap()
    );
    assert_eq!(
        crate::entity::agent_assistant_quota::Entity::find()
            .one(&store.db)
            .await
            .unwrap(),
        before
    );
    eprintln!(
        "assistant_memory_fixture facts=1000 recall_at_5=1/1 query_us={} model_calls=0",
        elapsed.as_micros()
    );
}
