use super::*;
use crate::{
    agent_run_event_store::{
        AppendUserFollowupParams, ReadContextSelection, SignalAgentRunEventStore,
    },
    agent_session_store::SignalAgentSessionStore,
    entity::host_remote_access_state,
};
use desk_agent_protocol::{AgentScope, ExecutionMode};
use desk_diagnose_core::{
    assistant_policy::PERSONAL_ASSISTANT_POLICY_REVISION,
    conversation_key::{AssistantConversationScope, derive_assistant_conversation_key},
    model_message_labels::model_bound_user_message,
    seam::{ClaimTurnParams, SessionSeam},
    session::{AgentSessionSurface, TriggerOrigin},
};

#[tokio::test]
async fn personal_input_selects_current_facts_by_device_and_rechecks_before_model_send() {
    let db = Database::connect("sqlite::memory:").await.unwrap();
    crate::db::initialize_schema(&db).await.unwrap();
    let store = TestStore { db };
    let (personal, _, _) = super::super::owner_memory::seed(&store, 1, "personal-fact").await;
    let mut devices = Vec::new();
    for label in ["selected-mac", "selected-linux", "unselected-device"] {
        let (memory, _, origin_id) = super::super::owner_memory::seed(&store, 1, label).await;
        let original = origin::Entity::find()
            .filter(origin::Column::Id.eq(origin_id))
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        let id = format!("offline-device-{label}");
        crate::device_code_store::get_or_create(&store.db, &id)
            .await
            .unwrap();
        fact::Entity::update_many()
            .col_expr(
                fact::Column::DeviceRef,
                Expr::value(original.device_ref.clone()),
            )
            .col_expr(
                fact::Column::ScopeKey,
                Expr::value(format!("device:{}", original.device_ref)),
            )
            .filter(fact::Column::MemoryId.eq(&memory))
            .exec(&store.db)
            .await
            .unwrap();
        devices.push((id, original.device_ref, memory));
    }
    let destination = super::super::model_sources::destination("model");
    let client_id = "selected-memory-client";
    let run =
        derive_assistant_conversation_key("1", AssistantConversationScope::Personal, client_id);
    let scope = AgentScope {
        granted: vec![],
        mode: ExecutionMode::ReadOnly,
        expires_at: None,
        policy_name: None,
    };
    let input = AppendUserFollowupParams {
        client_timezone: "UTC".into(),
        temporary_conversation: None,
        event_id: "selected-memory-event".into(),
        run_id: run.clone(),
        actor_id: "1".into(),
        device_id: String::new(),
        target_refs: devices
            .iter()
            .take(2)
            .map(|(id, _, _)| id.clone())
            .collect(),
        client_conversation_id: Some(client_id.into()),
        surface: AgentSessionSurface::AiAssistant,
        policy_revision: PERSONAL_ASSISTANT_POLICY_REVISION,
        read_context: Some(ReadContextSelection::capture(&[], &scope).unwrap()),
        current_scope: scope.clone(),
        message: model_bound_user_message(
            "selected-memory-input".into(),
            "Compare these devices".into(),
            destination.clone(),
        )
        .unwrap(),
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    let inputs = SignalAgentRunEventStore::new(store.db.clone());
    let receipt = inputs.append_user_followup(&input).await.unwrap();
    let session = SignalAgentSessionStore::new(store.db.clone())
        .with_client_metadata(
            input.client_conversation_id.clone(),
            AgentSessionSurface::AiAssistant,
        )
        .with_expected_input_revision(receipt.input_revision)
        .claim_turn(ClaimTurnParams {
            conversation_id: run,
            actor_id: "1".into(),
            device_id: String::new(),
            policy_revision: input.policy_revision,
            current_pdp_scope: scope,
            turn_id: "selected-memory-turn".into(),
            request_id: None,
            connection_id: None,
            trigger_origin: TriggerOrigin::User,
            now: input.created_at.clone(),
        })
        .await
        .unwrap();
    assert_eq!(
        session
            .assistant_data
            .as_ref()
            .unwrap()
            .request_memory_devices(),
        [devices[0].1.clone(), devices[1].1.clone()]
    );
    let fixture = Fixture {
        store,
        session: Box::new(session),
        destination,
    };
    assert_explicit_read_scope(
        &fixture,
        &personal,
        &devices[0].2,
        &devices[1].2,
        &devices[2].2,
    )
    .await;
    let (fixture, request, proof) = ready_request_for(fixture).await;
    let ids = proof
        .usage
        .memories
        .iter()
        .map(|memory| memory.memory_id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        ids,
        [
            personal.as_str(),
            devices[0].2.as_str(),
            devices[1].2.as_str()
        ]
        .into()
    );
    assert!(request.messages[0].text.contains("selected-mac"));
    assert!(request.messages[0].text.contains("selected-linux"));
    assert!(!request.messages[0].text.contains("unselected-device"));

    for before_revocation in [true, false] {
        if !before_revocation {
            host_remote_access_state::ActiveModel {
                client_id: Set(devices[0].0.clone()),
                locked: Set(true),
                state_version: Set(1),
                lock_id: Set(Some("selected-memory-lock".into())),
                updated_at: Set(chrono::Utc::now()),
            }
            .insert(&fixture.store.db)
            .await
            .unwrap();
        }
        let txn = fixture.store.db.begin().await.unwrap();
        let gate = super::super::super::model_gate::prepare_on(&txn, &request)
            .await
            .unwrap()
            .unwrap();
        super::super::model_gate::native_on(&txn, &request, &proof, "selected-memory-send").await;
        let result = super::super::super::model_gate::authorize_on(
            &txn,
            Some(&gate),
            &request,
            "selected-memory-send",
        )
        .await;
        assert_eq!(result.is_ok(), before_revocation, "{result:?}");
        txn.rollback().await.unwrap();
    }
    let remaining = current::prepare(
        &fixture.store.db,
        &fixture.session,
        &fixture.destination,
        "actual-current-send",
        8192,
    )
    .await
    .unwrap();
    let ids = remaining
        .records
        .iter()
        .map(|memory| memory.memory_id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(ids, [personal.as_str(), devices[1].2.as_str()].into());
    assert_ne!(remaining.uses[0].projection_id, proof.usage.projection_id);
}

async fn assert_explicit_read_scope(
    fixture: &Fixture,
    personal: &str,
    first: &str,
    second: &str,
    outside: &str,
) {
    use super::super::super::{memory_keyword, memory_view::MemoryReadAuthorization};
    use desk_agent_protocol::ai_assistant::{
        history::AssistantDeviceScope, memory::MemorySearchRequest,
    };
    use desk_diagnose_core::ai_assistant::{
        data_access::assistant_scope, memory_access::usable_memory_scope,
    };
    use std::collections::BTreeSet;

    for id in [personal, first, second, outside] {
        let row = fact::Entity::find()
            .filter(fact::Column::MemoryId.eq(id))
            .one(&fixture.store.db)
            .await
            .unwrap()
            .unwrap();
        super::super::memory_search::index_fact(&fixture.store, &row).await;
    }
    let txn = fixture.store.db.begin().await.unwrap();
    let now = chrono::Utc::now().timestamp_millis() as u64;
    let fence = current_native::prepare_on(&txn, &fixture.session, now)
        .await
        .unwrap();
    let scope = usable_memory_scope(&fixture.session, now).unwrap();
    let devices = current::fact_devices_on(&txn, &fixture.session, scope)
        .await
        .unwrap();
    let authority = MemoryReadAuthorization {
        actor_id: &fixture.session.actor_id,
        current_device_ref: &scope.current_device_ref,
        assistant_scope: &assistant_scope(&fixture.session).unwrap(),
        scope,
        access: &fence.access,
        platform: &fence.policy,
        destination: &fixture.destination,
        requested_devices: &devices,
        observation_generation: fence.access.memory_state_generation,
        now_unix_ms: now,
    };
    let search = MemorySearchRequest {
        queries: vec!["Chinese".into()],
        scope: AssistantDeviceScope::CurrentDevice,
        limit: 5,
        cursor: None,
    };
    let matches = memory_keyword::relevance_on(&txn, &authority, &search)
        .await
        .unwrap();
    assert_eq!(
        matches
            .versions
            .iter()
            .map(|version| version.source_id.as_str())
            .collect::<BTreeSet<_>>(),
        [personal, first, second].into(),
    );
    // The owner's other devices are reachable on request, without changing
    // the default selection above.
    assert_eq!(scope.device_rule, AssistantDeviceScope::AllMyDevices);
    let outside_device = fact::Entity::find()
        .filter(fact::Column::MemoryId.eq(outside))
        .one(&txn)
        .await
        .unwrap()
        .unwrap()
        .device_ref;
    let mut every = devices.clone();
    every.insert(outside_device);
    let wide = MemoryReadAuthorization {
        actor_id: authority.actor_id,
        current_device_ref: authority.current_device_ref,
        assistant_scope: authority.assistant_scope,
        scope,
        access: authority.access,
        platform: authority.platform,
        destination: authority.destination,
        requested_devices: &every,
        observation_generation: authority.observation_generation,
        now_unix_ms: now,
    };
    let all = memory_keyword::relevance_on(
        &txn,
        &wide,
        &MemorySearchRequest {
            scope: AssistantDeviceScope::AllMyDevices,
            ..search.clone()
        },
    )
    .await
    .unwrap();
    assert!(
        all.versions
            .iter()
            .any(|version| version.source_id == outside)
    );
    let current = memory_keyword::relevance_on(&txn, &wide, &search)
        .await
        .unwrap();
    assert!(
        !current
            .versions
            .iter()
            .any(|version| version.source_id == outside)
    );
    let result = desk_assistant_data_store::memory_history::head_on(&txn, &authority, second)
        .await
        .unwrap();
    assert_eq!(result.memory_id, second);
    assert!(
        desk_assistant_data_store::memory_history::head_on(&txn, &authority, outside)
            .await
            .is_err()
    );
    txn.rollback().await.unwrap();
}
