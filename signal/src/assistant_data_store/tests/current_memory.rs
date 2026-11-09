use super::super::{current_memory as current, current_native, projection};
use super::*;
use crate::entity::{
    agent_assistant_data_authorization as authorization, agent_assistant_origin as origin,
    agent_assistant_read_projection as projection_row, agent_memory as fact,
    agent_memory_source as evidence, agent_session, model_egress_receipt as native,
};
use desk_agent_protocol::{
    ai_assistant::source::SourceUsePolicy, data_lineage::DestinationIdentity,
};
use desk_diagnose_core::{
    ai_assistant::{current_memory::CurrentMemoryPayload, read_projection::ReadProjectionKind},
    session::PersistedAgentSession,
};
use sea_orm::{PaginatorTrait, sea_query::Expr};

mod native_send;
mod request_manifest;
mod selected_devices;

// Real owner/device/source and running-session graphs before the first native
// model send. Preparing this block must never itself create a native send grant.
struct Fixture {
    store: TestStore,
    session: Box<PersistedAgentSession>,
    destination: DestinationIdentity,
}
impl Fixture {
    async fn new(label: &str) -> Self {
        let (store, _, proof, _) = super::model_gate::fixture(label).await;
        let row = agent_session::Entity::find()
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        Self {
            store,
            session: Box::new(PersistedAgentSession::decode_json(&row.state_json).unwrap()),
            destination: proof.source_envelope.allowed_destinations[0].clone(),
        }
    }
}

/// This fixture CAS retains the original actor/version/lease. It publishes the
/// already stored preparation into a real session; it is not a new read grant.
async fn ready_request(
    label: &str,
) -> (
    Fixture,
    desk_diagnose_core::seam::ModelRequest,
    Box<desk_diagnose_core::ai_assistant::read_projection::ReadProjectionProof>,
) {
    ready_request_for(Fixture::new(label).await).await
}

async fn ready_request_for(
    mut fixture: Fixture,
) -> (
    Fixture,
    desk_diagnose_core::seam::ModelRequest,
    Box<desk_diagnose_core::ai_assistant::read_projection::ReadProjectionProof>,
) {
    let memory_host =
        crate::agent_session_store::SignalAgentSessionStore::new(fixture.store.db.clone())
            .with_client_metadata(
                None,
                desk_diagnose_core::session::AgentSessionSurface::AiAssistant,
            );
    let block = desk_diagnose_core::seam::SessionSeam::memory(&memory_host)
        .unwrap()
        .current_memories(
            &fixture.session,
            &fixture.destination,
            "actual-current-send",
            8192,
            chrono::Utc::now().timestamp_millis() as u64,
        )
        .await
        .unwrap();
    let usage = block.uses[0].clone();
    let envelope = block.source_envelopes[0].clone();
    let content = CurrentMemoryPayload::new(block.memory_state_generation, block.records)
        .encode(8192, chrono::Utc::now().timestamp_millis() as u64)
        .unwrap()
        .unwrap();
    let mut message = desk_diagnose_core::chat::ChatMessage::text(
        "actual-current-memory",
        desk_diagnose_core::chat::ChatRole::User,
        content,
    );
    message.data_envelope = Some(envelope.clone());
    let data = fixture.session.assistant_data.as_mut().unwrap();
    data.current_memories = usage.memories.clone();
    data.read_uses.push(usage.reference());
    for source in &usage.history_dependencies {
        if let Some(old) = data.history_dependencies.iter().find(|old| {
            old.source_id == source.source_id && old.source_revision == source.source_revision
        }) {
            assert_eq!(old, source);
        } else {
            data.history_dependencies.push(source.clone());
        }
    }
    let previous = fixture.session.version;
    fixture.session.version += 1;
    let changed = agent_session::Entity::update_many()
        .col_expr(
            agent_session::Column::Version,
            Expr::value(fixture.session.version),
        )
        .col_expr(
            agent_session::Column::StateJson,
            Expr::value(fixture.session.encode_json_for_storage().unwrap()),
        )
        .filter(agent_session::Column::ConversationId.eq(&fixture.session.conversation_id))
        .filter(agent_session::Column::ActorId.eq(&fixture.session.actor_id))
        .filter(agent_session::Column::Version.eq(previous))
        .filter(agent_session::Column::LeaseToken.eq(signed(fixture.session.lease_token).unwrap()))
        .exec(&fixture.store.db)
        .await
        .unwrap();
    assert_eq!(changed.rows_affected, 1);
    let data = fixture.session.assistant_data.as_ref().unwrap();
    let mut request = desk_diagnose_core::seam::ModelRequest::text_only(
        vec![message],
        desk_diagnose_core::prompt::ResponseFormatSpec::None,
    );
    request.context_conversation_id = Some(fixture.session.conversation_id.clone());
    request.assistant_data = Some(Box::new(
        desk_diagnose_core::ai_assistant::seam::AssistantModelDataManifest {
            actor_id: fixture.session.actor_id.clone(),
            conversation_id: fixture.session.conversation_id.clone(),
            source_device_ref: data.source_device_ref.clone(),
            request_memory_device_refs: data.request_memory_devices().to_vec(),
            session_version: fixture.session.version,
            lease_token: fixture.session.lease_token,
            input_revision: fixture.session.input_revision,
            actor_input_seq: data.current_actor_input_seq,
            retrieval_episode_id: data.retrieval_budget.episode_id.clone(),
            task_id: data.retrieval_task_id.clone(),
            policy_revision: usage.policy_revision,

            memory_state_generation: data.memory_state_generation,
            control_generation: data.control_generation,
            selected_envelopes: vec![
                desk_diagnose_core::ai_assistant::seam::SelectedAssistantEnvelope {
                    envelope_id: envelope.envelope_id,
                },
            ],
            read_projection_ids: vec![usage.projection_id.clone()],
            current_memories: usage.memories.clone(),
        },
    ));
    request
        .assistant_data
        .as_ref()
        .unwrap()
        .validate_for(&fixture.session, &request)
        .unwrap();
    let expected = request.assistant_data.take().unwrap();
    let host = crate::agent_session_store::SignalAgentSessionStore::new(fixture.store.db.clone());
    let generated = desk_diagnose_core::seam::SessionSeam::assistant_data_request_manifest(
        &host,
        &fixture.session,
        &request,
        chrono::Utc::now().timestamp_millis() as u64,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(&generated, expected.as_ref());
    request.assistant_data = Some(Box::new(generated));
    let stored = projection::load_on(
        &fixture.store.db,
        &fixture.session.actor_id,
        &usage.projection_id,
    )
    .await
    .unwrap()
    .unwrap();
    (fixture, request, stored.proof)
}

#[tokio::test]
async fn real_running_session_prepares_exact_current_facts_without_creating_or_consuming_a_native_send()
 {
    let mut fixture = Fixture::new("current-native-valid").await;
    fixture
        .session
        .assistant_data
        .as_mut()
        .unwrap()
        .retrieval_budget
        .limits
        .unused_projection_ttl_ms = 10_000;
    agent_session::Entity::update_many()
        .col_expr(
            agent_session::Column::StateJson,
            Expr::value(fixture.session.encode_json_for_storage().unwrap()),
        )
        .filter(agent_session::Column::ConversationId.eq(&fixture.session.conversation_id))
        .exec(&fixture.store.db)
        .await
        .unwrap();
    let host = crate::agent_session_store::SignalAgentSessionStore::new(fixture.store.db.clone())
        .with_client_metadata(
            None,
            desk_diagnose_core::session::AgentSessionSurface::AiAssistant,
        );
    let port = desk_diagnose_core::seam::SessionSeam::memory(&host).unwrap();
    let before = quota::Entity::find()
        .one(&fixture.store.db)
        .await
        .unwrap()
        .unwrap();
    let original = agent_session::Entity::find()
        .one(&fixture.store.db)
        .await
        .unwrap()
        .unwrap();
    let calls = native::Entity::find()
        .count(&fixture.store.db)
        .await
        .unwrap();
    let grants = authorization::Entity::find()
        .count(&fixture.store.db)
        .await
        .unwrap();
    let first = port
        .current_memories(
            &fixture.session,
            &fixture.destination,
            "preparation-A",
            8192,
            chrono::Utc::now().timestamp_millis() as u64,
        )
        .await
        .unwrap();
    assert_eq!(first.records.len(), 1);
    assert_eq!(first.source_envelopes.len(), 1);
    assert_eq!(first.uses.len(), 1);
    let now = chrono::Utc::now().timestamp_millis() as u64;
    let content = CurrentMemoryPayload::new(first.memory_state_generation, first.records.clone())
        .encode(8192, now)
        .unwrap()
        .unwrap();
    let row = projection_row::Entity::find()
        .filter(projection_row::Column::ProjectionId.eq(&first.uses[0].projection_id))
        .one(&fixture.store.db)
        .await
        .unwrap()
        .unwrap();
    let stored = projection::decode_row(row).unwrap();
    stored.proof.validate_cached_result(&content).unwrap();
    assert_eq!(
        stored.proof.unused_until_unix_ms,
        stored.proof.created_at_unix_ms + 10_000
    );
    assert_eq!(stored.proof.kind, ReadProjectionKind::CurrentMemories);
    assert_eq!(stored.proof.usage, first.uses[0]);
    assert_eq!(stored.proof.source_envelope, first.source_envelopes[0]);
    assert_eq!(stored.row.result_json.as_deref(), Some(content.as_str()));
    assert!(stored.row.first_consumed_at_ms.is_none());
    assert!(
        !fixture
            .session
            .assistant_data
            .as_ref()
            .unwrap()
            .read_uses
            .contains(&first.uses[0].reference())
    );
    let charged = quota::Entity::find()
        .one(&fixture.store.db)
        .await
        .unwrap()
        .unwrap();
    assert!(charged.metadata_bytes > before.metadata_bytes);
    let replay = port
        .current_memories(
            &fixture.session,
            &fixture.destination,
            "preparation-A",
            8192,
            chrono::Utc::now().timestamp_millis() as u64,
        )
        .await
        .unwrap();
    assert_eq!(replay.records, first.records);
    assert_eq!(replay.source_envelopes, first.source_envelopes);
    assert_eq!(replay.uses, first.uses);
    assert_eq!(
        quota::Entity::find()
            .one(&fixture.store.db)
            .await
            .unwrap()
            .unwrap(),
        charged
    );
    assert_eq!(
        agent_session::Entity::find()
            .one(&fixture.store.db)
            .await
            .unwrap()
            .unwrap(),
        original
    );
    assert_eq!(
        native::Entity::find()
            .count(&fixture.store.db)
            .await
            .unwrap(),
        calls
    );
    assert_eq!(
        authorization::Entity::find()
            .count(&fixture.store.db)
            .await
            .unwrap(),
        grants
    );
}

#[tokio::test]
async fn current_fact_selection_omits_conflicted_expired_and_unfittable_facts_without_charging_for_empty_blocks()
 {
    for variant in 0..3 {
        let fixture = Fixture::new(&format!("current-native-omit-{variant}")).await;
        let id = &fixture
            .session
            .assistant_data
            .as_ref()
            .unwrap()
            .current_memories[0]
            .memory_id;
        if variant == 0 {
            fact::Entity::update_many()
                .col_expr(fact::Column::Status, Expr::value("pending_conflict"))
                .filter(fact::Column::MemoryId.eq(id))
                .exec(&fixture.store.db)
                .await
                .unwrap();
        } else if variant == 1 {
            fact::Entity::update_many()
                .col_expr(fact::Column::ValidUntilMs, Expr::value(1_i64))
                .filter(fact::Column::MemoryId.eq(id))
                .exec(&fixture.store.db)
                .await
                .unwrap();
        }
        let before = quota::Entity::find()
            .one(&fixture.store.db)
            .await
            .unwrap()
            .unwrap();
        let proofs = projection_row::Entity::find()
            .count(&fixture.store.db)
            .await
            .unwrap();
        let block = current::prepare(
            &fixture.store.db,
            &fixture.session,
            &fixture.destination,
            "omit-preparation",
            if variant == 2 { 128 } else { 8192 },
        )
        .await
        .unwrap();
        assert!(
            block.records.is_empty() && block.source_envelopes.is_empty() && block.uses.is_empty()
        );
        assert_eq!(
            projection_row::Entity::find()
                .count(&fixture.store.db)
                .await
                .unwrap(),
            proofs
        );
        assert_eq!(
            quota::Entity::find()
                .one(&fixture.store.db)
                .await
                .unwrap()
                .unwrap(),
            before
        );
    }
}

#[tokio::test]
async fn a_source_policy_change_blocks_current_cache_replay_and_does_not_create_another_preparation()
 {
    let fixture = Fixture::new("current-native-policy").await;
    let block = current::prepare(
        &fixture.store.db,
        &fixture.session,
        &fixture.destination,
        "policy-preparation",
        8192,
    )
    .await
    .unwrap();
    let source = evidence::Entity::find()
        .filter(evidence::Column::SourceId.eq(&block.records[0].source_ids[0]))
        .one(&fixture.store.db)
        .await
        .unwrap()
        .unwrap();
    let original = origin::Entity::find()
        .filter(origin::Column::OriginId.eq(&source.origin_id))
        .one(&fixture.store.db)
        .await
        .unwrap()
        .unwrap();
    let mut policy: SourceUsePolicy = serde_json::from_str(&original.policy_json).unwrap();
    policy.no_export = true;
    origin::Entity::update_many()
        .col_expr(
            origin::Column::PolicyJson,
            Expr::value(serde_json::to_string(&policy).unwrap()),
        )
        .filter(origin::Column::OriginId.eq(original.origin_id))
        .exec(&fixture.store.db)
        .await
        .unwrap();
    let before = quota::Entity::find()
        .one(&fixture.store.db)
        .await
        .unwrap()
        .unwrap();
    let proofs = projection_row::Entity::find()
        .count(&fixture.store.db)
        .await
        .unwrap();
    assert!(
        current::prepare(
            &fixture.store.db,
            &fixture.session,
            &fixture.destination,
            "policy-preparation",
            8192
        )
        .await
        .is_err()
    );
    assert_eq!(
        quota::Entity::find()
            .one(&fixture.store.db)
            .await
            .unwrap()
            .unwrap(),
        before
    );
    assert_eq!(
        projection_row::Entity::find()
            .count(&fixture.store.db)
            .await
            .unwrap(),
        proofs
    );
}

#[tokio::test]
async fn original_missing_consent_and_temporary_conversations_never_load_account_facts() {
    for temporary in [false, true] {
        let mut fixture = Fixture::new(&format!("current-native-disabled-{temporary}")).await;
        let data = fixture.session.assistant_data.as_mut().unwrap();
        data.memory_access_scope = None;
        data.current_memories.clear();
        data.read_uses.clear();
        data.temporary = temporary;
        if temporary {
            data.history_access_scope = None;
            data.history_dependencies.clear();
        }
        agent_session::Entity::update_many()
            .col_expr(
                agent_session::Column::StateJson,
                Expr::value(fixture.session.encode_json_for_storage().unwrap()),
            )
            .filter(agent_session::Column::ConversationId.eq(&fixture.session.conversation_id))
            .exec(&fixture.store.db)
            .await
            .unwrap();
        let before = quota::Entity::find()
            .one(&fixture.store.db)
            .await
            .unwrap()
            .unwrap();
        let block = current::prepare(
            &fixture.store.db,
            &fixture.session,
            &fixture.destination,
            "no-original-consent",
            8192,
        )
        .await
        .unwrap();
        assert!(
            block.records.is_empty() && block.source_envelopes.is_empty() && block.uses.is_empty()
        );
        assert_eq!(
            quota::Entity::find()
                .one(&fixture.store.db)
                .await
                .unwrap()
                .unwrap(),
            before
        );
    }
}

#[tokio::test]
async fn current_preparation_final_projection_failure_rolls_back_metadata_and_preserves_original_session()
 {
    let fixture = Fixture::new("current-native-rollback").await;
    let before = quota::Entity::find()
        .one(&fixture.store.db)
        .await
        .unwrap()
        .unwrap();
    let original = agent_session::Entity::find()
        .one(&fixture.store.db)
        .await
        .unwrap()
        .unwrap();
    let proofs = projection_row::Entity::find()
        .count(&fixture.store.db)
        .await
        .unwrap();
    fixture.store.db.execute_unprepared("CREATE TRIGGER reject_current_memory BEFORE INSERT ON agent_assistant_read_projection BEGIN SELECT RAISE(ABORT, 'current-memory projection failure'); END").await.unwrap();
    assert!(
        current::prepare(
            &fixture.store.db,
            &fixture.session,
            &fixture.destination,
            "failed-current-preparation",
            8192
        )
        .await
        .is_err()
    );
    assert_eq!(
        quota::Entity::find()
            .one(&fixture.store.db)
            .await
            .unwrap()
            .unwrap(),
        before
    );
    assert_eq!(
        projection_row::Entity::find()
            .count(&fixture.store.db)
            .await
            .unwrap(),
        proofs
    );
    assert_eq!(
        agent_session::Entity::find()
            .one(&fixture.store.db)
            .await
            .unwrap()
            .unwrap(),
        original
    );
}

#[tokio::test]
async fn current_preparation_fence_reselects_original_raw_json_and_never_renews_its_initial_lease_bound()
 {
    {
        let fixture = Fixture::new("current-native-raw-fence").await;
        let txn = fixture.store.db.begin().await.unwrap();
        let now = chrono::Utc::now().timestamp_millis() as u64;
        let held = current_native::prepare_on(&txn, &fixture.session, now)
            .await
            .unwrap();
        let original = held.deadline;
        // Wall-clock adjustments do not change the original session authority.
        assert_eq!(held.finish_on(now - 82).await.unwrap(), original);
        assert!(held.finish_on(original).await.is_err());
        agent_session::Entity::update_many()
            .col_expr(
                agent_session::Column::LeaseDeadline,
                Expr::value(
                    chrono::DateTime::from_timestamp_millis((original + 60_000) as i64)
                        .unwrap()
                        .naive_utc(),
                ),
            )
            .filter(agent_session::Column::ConversationId.eq(&fixture.session.conversation_id))
            .exec(&txn)
            .await
            .unwrap();
        assert_eq!(
            held.finish_on(chrono::Utc::now().timestamp_millis() as u64)
                .await
                .unwrap(),
            original
        );
        let row = agent_session::Entity::find()
            .one(&txn)
            .await
            .unwrap()
            .unwrap();
        agent_session::Entity::update_many()
            .col_expr(
                agent_session::Column::StateJson,
                Expr::value(format!(" {} ", row.state_json)),
            )
            .filter(agent_session::Column::ConversationId.eq(&fixture.session.conversation_id))
            .exec(&txn)
            .await
            .unwrap();
        assert!(
            held.finish_on(chrono::Utc::now().timestamp_millis() as u64)
                .await
                .is_err()
        );
        txn.rollback().await.unwrap();
    }
}

#[tokio::test]
async fn history_retirement_fence_rejects_authority_changed_after_preparation() {
    for change_control in [false, true] {
        let fixture = Fixture::new(&format!("history-retirement-late-{change_control}")).await;
        let before = quota::Entity::find().one(&fixture.store.db).await.unwrap();
        let txn = fixture.store.db.begin().await.unwrap();
        let now = chrono::Utc::now().timestamp_millis() as u64;
        let fence = desk_diagnose_core::future::send_phase(|| {
            current_native::prepare_on(&txn, &fixture.session, now)
        })
        .await
        .unwrap();
        let runtime = runtime_on(&txn, &fixture.session.actor_id).await.unwrap();
        let (column, value) = if change_control {
            (
                runtime::Column::ControlGeneration,
                runtime.control_generation + 1,
            )
        } else {
            (
                runtime::Column::MemoryStateGeneration,
                runtime.memory_state_generation + 1,
            )
        };
        runtime::Entity::update_many()
            .col_expr(column, Expr::value(value))
            .filter(runtime::Column::Id.eq(runtime.id))
            .exec(&txn)
            .await
            .unwrap();
        assert!(
            fence
                .finish_on(chrono::Utc::now().timestamp_millis() as u64)
                .await
                .is_err()
        );
        txn.rollback().await.unwrap();
        assert_eq!(
            quota::Entity::find().one(&fixture.store.db).await.unwrap(),
            before
        );
    }
}

#[tokio::test]
async fn history_retirement_fence_rejects_generations_moving_backwards() {
    let mut fixture = Fixture::new("history-retirement-backwards").await;
    let data = fixture.session.assistant_data.as_mut().unwrap();
    data.memory_state_generation += 1;
    data.control_generation += 1;
    agent_session::Entity::update_many()
        .col_expr(
            agent_session::Column::StateJson,
            Expr::value(fixture.session.encode_json_for_storage().unwrap()),
        )
        .filter(agent_session::Column::ConversationId.eq(&fixture.session.conversation_id))
        .exec(&fixture.store.db)
        .await
        .unwrap();
    let before = quota::Entity::find().one(&fixture.store.db).await.unwrap();
    let txn = fixture.store.db.begin().await.unwrap();
    let now = chrono::Utc::now().timestamp_millis() as u64;
    assert!(
        current_native::prepare_on(&txn, &fixture.session, now,)
            .await
            .is_err()
    );
    txn.rollback().await.unwrap();
    assert_eq!(
        quota::Entity::find().one(&fixture.store.db).await.unwrap(),
        before
    );
}

#[tokio::test]
async fn current_memory_read_deadline_does_not_become_answer_retention_deadline() {
    let (fixture, _, original) = ready_request("current-memory-answer-lifetime").await;
    let mut proof = original.as_ref().clone();
    let natural = proof.created_at_unix_ms + 3_600_000;
    proof.usage.expires_at_unix_ms = Some(natural);
    proof.source_envelope.retention.expires_at_unix_ms = Some(natural);
    proof.validate().unwrap();
    let view = desk_diagnose_core::ai_assistant::retained_copy::OwnerHistoryReadValidation::for_prepared_projection(&proof).unwrap();
    let copy =
        super::super::owner_copy::inspect_on(&fixture.store.db, &view, proof.created_at_unix_ms)
            .await
            .unwrap()
            .unwrap();
    assert_eq!(
        proof.source_envelope.retention.expires_at_unix_ms,
        Some(natural)
    );
    assert_eq!(copy.policy.content_hard_expires_at_unix_ms, None);
}

#[tokio::test]
async fn current_memory_preparation_accepts_platform_capacity_above_the_old_fixed_limit() {
    let fixture = Fixture::new("current-platform-capacity").await;
    let block = current::prepare(
        &fixture.store.db,
        &fixture.session,
        &fixture.destination,
        "platform-capacity",
        64 * 1024,
    )
    .await
    .unwrap();
    assert!(!block.records.is_empty());
    let content = CurrentMemoryPayload::new(block.memory_state_generation, block.records)
        .encode(64 * 1024, chrono::Utc::now().timestamp_millis() as u64)
        .unwrap()
        .unwrap();
    let proof = projection::load_on(
        &fixture.store.db,
        &fixture.session.actor_id,
        &block.uses[0].projection_id,
    )
    .await
    .unwrap()
    .unwrap();
    proof.proof.validate_cached_result(&content).unwrap();
}
