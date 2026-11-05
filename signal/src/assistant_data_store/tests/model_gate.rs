use super::super::{model_gate, projection};
use super::*;
use crate::{
    config::connection::DatabaseTransaction,
    entity::{
        agent_assistant_data_authorization as authorization,
        agent_assistant_device_binding as device, agent_assistant_origin as origin,
        agent_assistant_quota as quota, agent_assistant_source_use as source_use,
        agent_memory_use as memory_use, agent_session, model_egress_receipt as native,
    },
};
use desk_agent_protocol::{AgentScope, ExecutionMode};
use desk_diagnose_core::{
    ai_assistant::{
        data_access::scope_sha256,
        read_projection::ReadProjectionProof,
        seam::{AssistantModelDataManifest, SelectedAssistantEnvelope},
    },
    chat::{ChatMessage, ChatRole},
    prompt::ResponseFormatSpec,
    seam::ModelRequest,
    session::{AgentSessionSurface, PersistedAgentSession, TurnState},
    sink_authorizer::SinkProjectionAudit,
};
use sea_orm::ExprTrait;
use sea_orm::{PaginatorTrait, sea_query::Expr};

pub(super) async fn fixture(label: &str) -> (TestStore, ModelRequest, ReadProjectionProof, String) {
    fixture_with_model(label, None).await
}

pub(super) async fn fixture_with_model(
    label: &str,
    model_base_url: Option<&str>,
) -> (TestStore, ModelRequest, ReadProjectionProof, String) {
    let store = setup().await;
    let actual_destination = if let Some(base_url) = model_base_url {
        Some(
            super::model_provider::configure_model(&store.db, base_url)
                .await
                .destination_identity()
                .unwrap(),
        )
    } else {
        None
    };
    let (memory, source, origin_id) = super::owner_memory::seed(&store, 1, label).await;
    let original = origin::Entity::find()
        .filter(origin::Column::OriginId.eq(&origin_id))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let device = device::Entity::find()
        .filter(device::Column::BindingId.eq(&original.device_ref))
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    let now = chrono::Utc::now();
    let mut session = PersistedAgentSession::new(
        "root",
        "1",
        device.audience.clone(),
        1,
        AgentScope {
            granted: Vec::new(),
            mode: ExecutionMode::ReadOnly,
            expires_at: None,
            policy_name: None,
        },
        &now.to_rfc3339(),
    );
    session.surface = AgentSessionSurface::AiAssistant;
    session.version = 1;
    session.lease_token = 1;
    session.turn_state = TurnState::Running;
    session.current_turn_id = Some("native-turn".into());
    let policy = AssistantDataPolicy::default();
    let txn = store.db.begin().await.unwrap();
    ensure_runtime_on(&txn, "1").await.unwrap();
    accept_input_on(
        &txn,
        &mut session,
        "UTC",
        None,
        now.timestamp_millis() as u64,
    )
    .await
    .unwrap();
    session.input_revision = 1;
    session.latest_input_seq = 1;
    session.begin_focus_epoch(1, Vec::new()).unwrap();
    txn.commit().await.unwrap();
    let mut selected = super::model_sources::memory_usage(&store, &memory, &source).await;
    selected.scope_sha256 = scope_sha256(&session).unwrap();
    selected.retrieval_episode_id = session
        .assistant_data
        .as_ref()
        .unwrap()
        .retrieval_budget
        .episode_id
        .clone();
    let access = super::model_sources::access(&store).await;
    let payload = desk_diagnose_core::ai_assistant::current_memory::CurrentMemoryPayload::new(
        access.memory_state_generation,
        vec![desk_agent_protocol::ai_assistant::memory::MemoryRecord {
            memory_id: memory.clone(),
            revision: 1,
            scope: desk_agent_protocol::ai_assistant::memory::MemoryScope::Personal,
            source_device: None,

            content: format!("Prefer Chinese for {label}"),
            context: Some("Owner preference".into()),
            status: desk_agent_protocol::ai_assistant::memory::MemoryStatus::Active,
            source_ids: vec![source],
            observed_at_unix_ms: 1,
            updated_at_unix_ms: 1,
            valid_from_unix_ms: 1,
            valid_until_unix_ms: None,
        }],
    );
    let content = payload
        .encode(8192, now.timestamp_millis() as u64)
        .unwrap()
        .unwrap();
    let destination =
        actual_destination.unwrap_or_else(|| super::model_sources::destination("model"));
    let mut proof = ReadProjectionProof::for_current_memories(
        desk_diagnose_core::ai_assistant::read_projection::CurrentMemoryPreparation {
            session: &session,
            access: &access,
            destination: &destination,
            preparation_id: label,
            maximum_bytes: 8192,
            authority_deadline: now.timestamp_millis() as u64 + 60_000,
            now: now.timestamp_millis() as u64,
        },
        &content,
        &[selected],
    )
    .unwrap();
    let copy_view = desk_diagnose_core::ai_assistant::retained_copy::OwnerHistoryReadValidation::for_prepared_projection(&proof).unwrap();
    proof.snapshot_policy =
        super::super::owner_copy::inspect_on(&store.db, &copy_view, now.timestamp_millis() as u64)
            .await
            .unwrap()
            .map(|copy| copy.policy);
    let mut message = ChatMessage::text("current-memories", ChatRole::User, content.clone());
    message.data_envelope = Some(proof.source_envelope.clone());
    let data = session.assistant_data.as_mut().unwrap();
    data.current_memories = proof.usage.memories.clone();
    data.read_uses.push(proof.usage.reference());
    data.history_dependencies = proof.usage.history_dependencies.clone();
    let txn = store.db.begin().await.unwrap();
    projection::prepare_on(&txn, &proof, &content, &policy, proof.created_at_unix_ms)
        .await
        .unwrap();
    agent_session::ActiveModel {
        conversation_schema_version: Set(Some(i32::from(
            desk_diagnose_core::session::CONVERSATION_SCHEMA_VERSION,
        ))),
        conversation_id: Set(session.conversation_id.clone()),
        actor_id: Set(session.actor_id.clone()),
        device_id: Set(session.device_id.clone()),
        state_json: Set(session.encode_json_for_storage().unwrap()),
        version: Set(session.version),
        lease_token: Set(session.lease_token as i64),
        lease_deadline: Set(Some(now + chrono::Duration::seconds(90))),
        created_at: Set(now),
        updated_at: Set(now),
        ..Default::default()
    }
    .insert(&txn)
    .await
    .unwrap();
    txn.commit().await.unwrap();
    let mut request = ModelRequest::text_only(vec![message], ResponseFormatSpec::None);
    request.context_conversation_id = Some(session.conversation_id.clone());
    let data = session.assistant_data.as_ref().unwrap();
    request.assistant_data = Some(Box::new(AssistantModelDataManifest {
        actor_id: session.actor_id.clone(),
        conversation_id: session.conversation_id.clone(),
        source_device_ref: data.source_device_ref.clone(),
        request_memory_device_refs: data.request_memory_devices().to_vec(),
        session_version: session.version,
        lease_token: session.lease_token,
        input_revision: session.input_revision,
        actor_input_seq: data.current_actor_input_seq,
        retrieval_episode_id: data.retrieval_budget.episode_id.clone(),
        task_id: None,
        scope_sha256: scope_sha256(&session).unwrap(),
        policy_revision: 1,

        memory_state_generation: data.memory_state_generation,
        control_generation: data.control_generation,
        selected_envelopes: vec![SelectedAssistantEnvelope {
            envelope_id: proof.source_envelope.envelope_id.clone(),
            digest_sha256: proof.source_envelope.digest_sha256.clone(),
        }],
        read_projection_ids: vec![proof.usage.projection_id.clone()],
        current_memories: data.current_memories.clone(),
    }));
    (store, request, proof, origin_id)
}

// Native receipt storage is real; provider I/O is intentionally outside these
// transaction tests. Existing dispatch admission still owns business authority.
pub(super) async fn native_on(
    db: &DatabaseTransaction,
    request: &ModelRequest,
    proof: &ReadProjectionProof,
    call: &str,
) {
    let envelopes = request
        .messages
        .iter()
        .filter_map(|message| message.data_envelope.clone())
        .collect::<Vec<_>>();
    let sink = SinkProjectionAudit {
        destination: proof.source_envelope.allowed_destinations[0].clone(),
        envelope_ids: envelopes
            .iter()
            .map(|value| value.envelope_id.clone())
            .collect(),
        digests_sha256: envelopes
            .iter()
            .map(|value| value.digest_sha256.clone())
            .collect(),
        total_bytes: envelopes
            .iter()
            .map(|value| match value.content {
                desk_agent_protocol::data_lineage::ContentRef::ImmutableBlob {
                    size_bytes, ..
                } => size_bytes as usize,
                _ => unreachable!(),
            })
            .sum(),
    };
    crate::model_egress_store::SignalModelEgressStore::record_dispatch_intent_on(
        db,
        call.into(),
        "export".into(),
        1,
        &sink,
        &envelopes,
    )
    .await
    .unwrap();
    crate::model_egress_store::SignalModelEgressStore::link_context_dispatch_on(
        db,
        call,
        request.context_conversation_id.as_deref().unwrap(),
        false,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn direct_send_records_live_sources_only_with_its_original_native_receipt() {
    let (store, request, proof, _) = fixture("native-gate").await;
    let txn = store.db.begin().await.unwrap();
    let prepared = model_gate::prepare_on(&txn, &request)
        .await
        .unwrap()
        .unwrap();
    native_on(&txn, &request, &proof, "physical-gate").await;
    let deadline = model_gate::authorize_on(&txn, Some(&prepared), &request, "physical-gate")
        .await
        .unwrap()
        .unwrap();
    assert!(deadline > chrono::Utc::now().timestamp_millis() as u64);
    assert!(deadline <= chrono::Utc::now().timestamp_millis() as u64 + 90_000);
    assert_eq!(authorization::Entity::find().count(&txn).await.unwrap(), 1);
    assert_eq!(source_use::Entity::find().count(&txn).await.unwrap(), 1);
    assert_eq!(memory_use::Entity::find().count(&txn).await.unwrap(), 1);
    let native_row = native::Entity::find().one(&txn).await.unwrap().unwrap();
    let set = desk_diagnose_core::ai_assistant::native_export::AssistantNativeSourceSet::decode(
        native_row.assistant_source_set_json.as_deref().unwrap(),
    )
    .unwrap();
    assert_eq!(set.authorizations.len(), 1);
    let before_replay = quota::Entity::find().one(&txn).await.unwrap().unwrap();
    let grants = vec![
        *super::super::native_export::decode_row(
            authorization::Entity::find()
                .one(&txn)
                .await
                .unwrap()
                .unwrap(),
        )
        .unwrap()
        .proof,
    ];
    super::super::native_export::record_call_on(&txn, &grants, &AssistantDataPolicy::default())
        .await
        .unwrap();
    assert_eq!(
        quota::Entity::find().one(&txn).await.unwrap().unwrap(),
        before_replay
    );
    assert!(
        projection::load_on(&txn, "1", &proof.usage.projection_id)
            .await
            .unwrap()
            .unwrap()
            .row
            .first_consumed_at_ms
            .is_some()
    );
    txn.commit().await.unwrap();
    assert_eq!(native::Entity::find().count(&store.db).await.unwrap(), 1);
}

#[tokio::test]
async fn a_source_revoked_after_preparation_rolls_back_the_native_receipt_and_every_use() {
    let (store, request, proof, original) = fixture("revoked-gate").await;
    let before = quota::Entity::find().one(&store.db).await.unwrap().unwrap();
    let txn = store.db.begin().await.unwrap();
    let prepared = model_gate::prepare_on(&txn, &request)
        .await
        .unwrap()
        .unwrap();
    native_on(&txn, &request, &proof, "physical-revoked").await;
    origin::Entity::update_many()
        .col_expr(
            origin::Column::DeletedAtMs,
            Expr::value(Some(chrono::Utc::now().timestamp_millis())),
        )
        .filter(origin::Column::OriginId.eq(original))
        .exec(&txn)
        .await
        .unwrap();
    assert!(
        model_gate::authorize_on(&txn, Some(&prepared), &request, "physical-revoked")
            .await
            .is_err()
    );
    txn.rollback().await.unwrap();
    assert_eq!(native::Entity::find().count(&store.db).await.unwrap(), 0);
    assert_eq!(
        authorization::Entity::find()
            .count(&store.db)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        source_use::Entity::find().count(&store.db).await.unwrap(),
        0
    );
    assert_eq!(
        memory_use::Entity::find().count(&store.db).await.unwrap(),
        0
    );
    assert_eq!(
        quota::Entity::find().one(&store.db).await.unwrap().unwrap(),
        before
    );
    assert!(
        projection::load_on(&store.db, "1", &proof.usage.projection_id)
            .await
            .unwrap()
            .unwrap()
            .row
            .first_consumed_at_ms
            .is_none()
    );
}

#[tokio::test]
async fn the_final_select_rejects_changed_raw_json_even_with_the_same_version_and_lease() {
    let (store, request, proof, _) = fixture("raw-json-gate").await;
    let txn = store.db.begin().await.unwrap();
    let prepared = model_gate::prepare_on(&txn, &request)
        .await
        .unwrap()
        .unwrap();
    native_on(&txn, &request, &proof, "physical-json").await;
    let row = agent_session::Entity::find()
        .one(&txn)
        .await
        .unwrap()
        .unwrap();
    let mut changed: agent_session::ActiveModel = row.into();
    let raw = match &changed.state_json {
        sea_orm::ActiveValue::Unchanged(raw) => raw.clone(),
        _ => unreachable!(),
    };
    changed.state_json = Set(format!(" {raw}"));
    changed.update(&txn).await.unwrap();
    assert!(
        model_gate::authorize_on(&txn, Some(&prepared), &request, "physical-json")
            .await
            .is_err()
    );
    txn.rollback().await.unwrap();
    assert_eq!(native::Entity::find().count(&store.db).await.unwrap(), 0);
}

#[tokio::test]
async fn an_unselected_read_or_changed_generation_never_creates_a_physical_source_grant() {
    let (store, request, proof, _) = fixture("actual-input-gate").await;
    for case in 0..2 {
        let txn = store.db.begin().await.unwrap();
        let prepared = model_gate::prepare_on(&txn, &request)
            .await
            .unwrap()
            .unwrap();
        let mut actual = proof.clone();
        let mut actual_request = request.clone();
        if case == 0 {
            actual.source_envelope.envelope_id = "other-actual-input".into();
            actual_request.messages[0].data_envelope = Some(actual.source_envelope.clone());
        }
        native_on(&txn, &actual_request, &actual, "physical-different-input").await;
        if case == 1 {
            runtime::Entity::update_many()
                .col_expr(
                    runtime::Column::ControlGeneration,
                    Expr::col(runtime::Column::ControlGeneration).add(1_i64),
                )
                .filter(runtime::Column::ActorId.eq("1"))
                .exec(&txn)
                .await
                .unwrap();
        }
        assert!(
            model_gate::authorize_on(&txn, Some(&prepared), &request, "physical-different-input")
                .await
                .is_err()
        );
        txn.rollback().await.unwrap();
    }
    assert_eq!(native::Entity::find().count(&store.db).await.unwrap(), 0);
    assert_eq!(
        authorization::Entity::find()
            .count(&store.db)
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn a_direct_read_cannot_be_omitted_from_the_request_manifest() {
    let (store, mut request, _, _) = fixture("missing-proof-gate").await;
    request
        .assistant_data
        .as_mut()
        .unwrap()
        .read_projection_ids
        .clear();
    let txn = store.db.begin().await.unwrap();
    assert!(model_gate::prepare_on(&txn, &request).await.is_err());
    txn.rollback().await.unwrap();
    assert_eq!(native::Entity::find().count(&store.db).await.unwrap(), 0);
}
