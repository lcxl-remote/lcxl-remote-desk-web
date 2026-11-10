use super::*;
use desk_agent_protocol::ai_assistant::{history::HistoryTimeRange, source::AssistantSourceKind};

fn destination(model: &str) -> DestinationIdentity {
    DestinationIdentity::Model {
        connection_id: "gateway".into(),
        connection_revision: 1,
        model_id: model.into(),
        profile_revision: 1,
    }
}
fn binding() -> RetrievalQueryBinding {
    RetrievalQueryBinding {
        actor_id: "actor".into(),
        root_conversation_id: "root".into(),
        retrieval_episode_id: "episode".into(),
        source_device_ref: "device-A".into(),
        task_id: None,
        scope: Box::default(),
        history_scope: Some(Box::new(HistoryAccessScope {
            actor_id: "actor".into(),
            root_conversation_id: "root".into(),
            task_id: None,
            authorized_input_seq: 1,
            accepted_at_unix_ms: 100,
            device_rule: AssistantDeviceScope::AllMyDevices,
            current_device_ref: "device-A".into(),
            allowed_kinds: vec![AssistantSourceKind::Input],
            time_range: None,
            timezone: "Asia/Shanghai".into(),
            policy_revision: 1,
            expires_at_unix_ms: None,
            retrieval_episode_id: "episode".into(),
        })),
        memory_device_rule: Some(AssistantDeviceScope::AllMyDevices),
        request_memory_device_refs: Vec::new(),
        destination: destination("first"),
        authority_expires_at_unix_ms: None,
    }
}
fn request() -> FrozenRetrievalRequest {
    FrozenRetrievalRequest::Conversations { request: serde_json::from_value(serde_json::json!({
        "queries": ["REPORT 邮件"], "scope": "all_my_devices", "time_range": null, "cursor": null,
    })).unwrap() }
}
fn reservation(kind: RetrievalCallKind) -> RetrievalReservation {
    RetrievalReservation {
        logical_call_id: "logical-call".into(),
        request_sha256: "b".repeat(64),
        kind,
        reserved_at_unix_ms: 1000,
        reserved_bytes: if kind == RetrievalCallKind::Relevance {
            16384
        } else {
            32768
        },
        actual_bytes: None,
    }
}
fn source(label: &str, revision: u64) -> QuerySourceVersion {
    QuerySourceVersion {
        source_id: native_ids::fixture_source_id(&label),
        revision,
    }
}
fn selection() -> RetrievalSelection {
    RetrievalSelection {
        devices: vec!["device-B".into(), "device-A".into()],
        watermark: RetrievalWatermark {
            corpus_generation: 10,
            memory_generation: 1,
            control_generation: 1,
            policy_revision: 1,
        },
        candidates: vec![source("one", 1), source("two", 1)],
        reasons: vec!["candidate_statistics".into()],
    }
}
fn query() -> FrozenRetrievalQuery {
    FrozenRetrievalQuery::prepare(
        binding(),
        &reservation(RetrievalCallKind::Relevance),
        request(),
        selection(),
        1001,
        desk_agent_protocol::ai_assistant::limits::RetrievalEpisodeLimits {
            query_ttl_ms: desk_agent_protocol::ai_assistant::limits::DEFAULT_QUERY_TTL_MS,
            ..Default::default()
        },
    )
    .unwrap()
}
fn token(label: &str) -> String {
    native_ids::stable(Kind::AssistantPageCursor, &label)
}

#[test]
fn query_lifetime_begins_at_original_admission_even_after_process_recovery_or_cache_removal() {
    let original = reservation(RetrievalCallKind::Relevance);
    let recovered: RetrievalReservation =
        serde_json::from_str(&serde_json::to_string(&original).unwrap()).unwrap();
    let later = FrozenRetrievalQuery::prepare(
        binding(),
        &recovered,
        request(),
        selection(),
        20_000,
        desk_agent_protocol::ai_assistant::limits::RetrievalEpisodeLimits {
            query_ttl_ms: desk_agent_protocol::ai_assistant::limits::DEFAULT_QUERY_TTL_MS,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(later.snapshot.captured_at_unix_ms, 1000);
    assert_eq!(
        later.snapshot.expires_at_unix_ms,
        1000 + desk_agent_protocol::ai_assistant::limits::DEFAULT_QUERY_TTL_MS
    );
    assert_eq!(later.refreshed_at_unix_ms, 20_000);
    assert_eq!(later.source_observed_at_unix_ms, 20_000);
    let expired = 1000 + desk_agent_protocol::ai_assistant::limits::DEFAULT_QUERY_TTL_MS;
    assert!(
        FrozenRetrievalQuery::prepare(
            binding(),
            &recovered,
            request(),
            selection(),
            expired,
            desk_agent_protocol::ai_assistant::limits::RetrievalEpisodeLimits {
                query_ttl_ms: desk_agent_protocol::ai_assistant::limits::DEFAULT_QUERY_TTL_MS,
                ..Default::default()
            },
        )
        .is_err()
    );
    assert!(
        FrozenRetrievalQuery::prepare(
            binding(),
            &recovered,
            request(),
            selection(),
            expired + 1,
            desk_agent_protocol::ai_assistant::limits::RetrievalEpisodeLimits {
                query_ttl_ms: desk_agent_protocol::ai_assistant::limits::DEFAULT_QUERY_TTL_MS,
                ..Default::default()
            },
        )
        .is_err()
    );
    let mut settled = recovered.clone();
    settled.actual_bytes = Some(100);
    assert!(
        FrozenRetrievalQuery::prepare(
            binding(),
            &settled,
            request(),
            selection(),
            1100,
            desk_agent_protocol::ai_assistant::limits::RetrievalEpisodeLimits {
                query_ttl_ms: desk_agent_protocol::ai_assistant::limits::DEFAULT_QUERY_TTL_MS,
                ..Default::default()
            },
        )
        .is_err()
    );
    assert!(
        FrozenRetrievalQuery::prepare(
            binding(),
            &reservation(RetrievalCallKind::Read),
            request(),
            selection(),
            1100,
            desk_agent_protocol::ai_assistant::limits::RetrievalEpisodeLimits {
                query_ttl_ms: desk_agent_protocol::ai_assistant::limits::DEFAULT_QUERY_TTL_MS,
                ..Default::default()
            },
        )
        .is_err()
    );
}

#[test]
fn original_task_and_history_deadlines_only_shorten_the_original_query() {
    let mut task = binding();
    task.task_id = Some("goal".into());
    task.authority_expires_at_unix_ms = Some(1500);
    let scope = task.history_scope.as_mut().unwrap();
    scope.task_id = task.task_id.clone();
    scope.expires_at_unix_ms = Some(1200);
    let query = FrozenRetrievalQuery::prepare(
        task.clone(),
        &reservation(RetrievalCallKind::Relevance),
        request(),
        selection(),
        1100,
        desk_agent_protocol::ai_assistant::limits::RetrievalEpisodeLimits {
            query_ttl_ms: desk_agent_protocol::ai_assistant::limits::DEFAULT_QUERY_TTL_MS,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(query.snapshot.expires_at_unix_ms, 1200);
    task.authority_expires_at_unix_ms = None;
    assert!(task.validate().is_err());
    let mut changed = query.clone();
    changed.snapshot.expires_at_unix_ms = 1500;
    assert!(changed.validate().is_err());
    let mut changed = binding();
    changed.history_scope.as_mut().unwrap().actor_id = "another-owner".into();
    assert!(changed.validate().is_err());
}

#[test]
fn equivalent_keyword_sets_resume_the_same_query_but_changed_arguments_do_not() {
    let mut query = query();
    let cursor = query
        .add_page(token("next"), PageAnchor::Relevance { next_offset: 1 })
        .unwrap();
    let mut equivalent = request();
    if let FrozenRetrievalRequest::Conversations { request } = &mut equivalent {
        request.queries = vec!["邮件 report".into(), "REPORT 邮件".into()];
        request.cursor = Some(cursor.clone());
    }
    assert_eq!(
        query.page(&cursor, &binding(), &equivalent, 1100).unwrap(),
        &PageAnchor::Relevance { next_offset: 1 }
    );
    let mut narrower = equivalent;
    if let FrozenRetrievalRequest::Conversations { request } = &mut narrower {
        request.limit = 1;
    }
    assert!(query.page(&cursor, &binding(), &narrower, 1100).is_err());
    assert!(
        query
            .page(
                &cursor,
                &binding(),
                &request(),
                query.snapshot.expires_at_unix_ms
            )
            .is_err()
    );
}

#[test]
fn cursor_locator_never_replaces_actor_root_episode_scope_task_or_recipient_checks() {
    let mut query = query();
    let cursor = query
        .add_page(token("next"), PageAnchor::Relevance { next_offset: 1 })
        .unwrap();
    let mut alternatives = Vec::new();
    let mut other = binding();
    other.actor_id = "other".into();
    alternatives.push(other);
    let mut other = binding();
    other.root_conversation_id = "other".into();
    alternatives.push(other);
    let mut other = binding();
    other.retrieval_episode_id = "other".into();
    alternatives.push(other);
    let mut other = binding();
    other.scope.instructions_revision = 9;
    alternatives.push(other);
    let mut other = binding();
    other.source_device_ref = "device-B".into();
    alternatives.push(other);
    let mut other = binding();
    other.destination = destination("another-model");
    alternatives.push(other);
    let mut other = binding();
    other.task_id = Some("task".into());
    alternatives.push(other);
    for other in alternatives {
        assert!(query.page(&cursor, &other, &request(), 1100).is_err());
    }
    let forged = QueryCursor::new(&query.snapshot.query_id, &token("unknown"))
        .unwrap()
        .encode();
    assert!(query.page(&forged, &binding(), &request(), 1100).is_err());
    let another_id = native_ids::stable(Kind::AssistantRetrievalQuery, &"other");
    let forged = QueryCursor::new(&another_id, &token("next"))
        .unwrap()
        .encode();
    assert!(query.page(&forged, &binding(), &request(), 1100).is_err());
    for bad in [
        String::new(),
        token("page-alone"),
        format!("{cursor}.extra"),
        "x".repeat(129),
    ] {
        assert!(QueryCursor::decode(&bad).is_err());
    }
    assert!(QueryCursor::new(&query.snapshot.query_id, &query.snapshot.query_id).is_err());
}

#[test]
fn a_personal_memory_query_preserves_its_selected_devices_across_pages() {
    let mut original = binding();
    original.source_device_ref.clear();
    original.history_scope = None;
    original.memory_device_rule = Some(AssistantDeviceScope::CurrentDevice);
    original.request_memory_device_refs = vec!["device-A".into(), "device-B".into()];
    let request = FrozenRetrievalRequest::Memories {
        request: serde_json::from_value(serde_json::json!({"queries": ["settings"]})).unwrap(),
    };
    let mut query = FrozenRetrievalQuery::prepare(
        original.clone(),
        &reservation(RetrievalCallKind::Relevance),
        request.clone(),
        selection(),
        1100,
        Default::default(),
    )
    .unwrap();
    assert_eq!(query.snapshot.device_refs, ["device-A", "device-B"]);

    let mut narrowed = selection();
    narrowed.devices = vec!["device-B".into()];
    query.refresh(narrowed, 1200).unwrap();
    assert_eq!(query.snapshot.device_refs, ["device-B"]);
    assert!(query.refresh(selection(), 1300).is_err());

    let mut outside = selection();
    outside.devices.push("device-C".into());
    assert!(
        FrozenRetrievalQuery::prepare(
            original,
            &reservation(RetrievalCallKind::Relevance),
            request,
            outside,
            1100,
            Default::default(),
        )
        .is_err()
    );
}

#[test]
fn window_and_memory_queries_preserve_purpose_and_original_memory_upper_scope() {
    let memory = FrozenRetrievalRequest::Memories {
        request: serde_json::from_value(serde_json::json!({
            "queries": ["环境"], "scope": "all_my_devices", "cursor": null,
        }))
        .unwrap(),
    };
    let mut limited = binding();
    limited.memory_device_rule = Some(AssistantDeviceScope::CurrentDevice);
    assert!(
        FrozenRetrievalQuery::prepare(
            limited,
            &reservation(RetrievalCallKind::Relevance),
            memory.clone(),
            selection(),
            1100,
            desk_agent_protocol::ai_assistant::limits::RetrievalEpisodeLimits {
                query_ttl_ms: desk_agent_protocol::ai_assistant::limits::DEFAULT_QUERY_TTL_MS,
                ..Default::default()
            },
        )
        .is_err()
    );
    let mut absent = binding();
    absent.memory_device_rule = None;
    assert!(
        FrozenRetrievalQuery::prepare(
            absent,
            &reservation(RetrievalCallKind::Relevance),
            memory.clone(),
            selection(),
            1100,
            desk_agent_protocol::ai_assistant::limits::RetrievalEpisodeLimits {
                query_ttl_ms: desk_agent_protocol::ai_assistant::limits::DEFAULT_QUERY_TTL_MS,
                ..Default::default()
            },
        )
        .is_err()
    );
    let memories = FrozenRetrievalQuery::prepare(
        binding(),
        &reservation(RetrievalCallKind::Relevance),
        memory,
        selection(),
        1100,
        desk_agent_protocol::ai_assistant::limits::RetrievalEpisodeLimits {
            query_ttl_ms: desk_agent_protocol::ai_assistant::limits::DEFAULT_QUERY_TTL_MS,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(memories.snapshot.purpose, SourceReadPurpose::ModelMemory);
    let anchor = source("anchor", 1);
    let window = FrozenRetrievalRequest::Window {
        request: ConversationReadRequest {
            source_id: anchor.source_id.clone(),
            view: ConversationReadView::Latest,
            limit: 10,
            max_bytes: 32768,
            cursor: None,
        },
        anchor,
        conversation_id: "past-root".into(),
    };
    let mut query = FrozenRetrievalQuery::prepare(
        binding(),
        &reservation(RetrievalCallKind::Read),
        window.clone(),
        selection(),
        1100,
        desk_agent_protocol::ai_assistant::limits::RetrievalEpisodeLimits {
            query_ttl_ms: desk_agent_protocol::ai_assistant::limits::DEFAULT_QUERY_TTL_MS,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(query.snapshot.purpose, SourceReadPurpose::ModelHistory);
    assert!(
        query
            .add_page(
                token("wrong-mode"),
                PageAnchor::Relevance { next_offset: 1 }
            )
            .is_err()
    );
    let cursor = query
        .add_page(
            token("window"),
            PageAnchor::Window {
                admitted_generation: 10,
                fragment_index: 0,
                history_id: 3,
                forward: true,
            },
        )
        .unwrap();
    assert!(query.page(&cursor, &binding(), &window, 1200).is_ok());
    let mut different_anchor = window;
    if let FrozenRetrievalRequest::Window { anchor, .. } = &mut different_anchor {
        anchor.revision = 2;
    }
    assert!(
        query
            .page(&cursor, &binding(), &different_anchor, 1200)
            .is_err()
    );
}

#[test]
fn refresh_shrinks_scope_resets_page_positions_and_preserves_seen_versions_and_expiry() {
    let mut old = query();
    old.remember(&[source("one", 1)]).unwrap();
    let cursor = old
        .add_page(token("next"), PageAnchor::Relevance { next_offset: 1 })
        .unwrap();
    let mut next = old.clone();
    let mut changed = selection();
    changed.devices = vec!["device-A".into()];
    changed.watermark.control_generation = 2;
    changed.candidates = vec![source("one", 1), source("new", 1)];
    next.refresh(changed, 2000).unwrap();
    old.validate_successor(&next).unwrap();
    assert_eq!(
        next.snapshot.captured_at_unix_ms,
        old.snapshot.captured_at_unix_ms
    );
    assert_eq!(
        next.snapshot.expires_at_unix_ms,
        old.snapshot.expires_at_unix_ms
    );
    assert_eq!(
        next.source_observation_generation,
        old.source_observation_generation
    );
    assert_eq!(
        next.source_observed_at_unix_ms,
        old.source_observed_at_unix_ms
    );
    assert!(next.was_seen(&source("one", 1)));
    assert!(!next.was_seen(&source("one", 2)));
    assert!(next.reasons.contains(&"scope_changed".into()));
    assert!(next.reasons.contains(&"query_refreshed".into()));
    assert_eq!(
        next.page(&cursor, &binding(), &request(), 2100).unwrap(),
        &PageAnchor::Start
    );
    assert!(next.refresh(selection(), 2200).is_err());
    let mut expanding = selection();
    expanding.watermark.control_generation = 3;
    expanding.devices.push("device-C".into());
    let before = next.clone();
    assert!(next.refresh(expanding, 2200).is_err());
    assert_eq!(next, before);
    let mut backwards = next.watermark();
    backwards.corpus_generation -= 1;
    assert!(next.needs_refresh(backwards).is_err());
}

#[test]
fn successors_cannot_silently_requery_renew_restore_scope_or_discard_issued_cursors() {
    let mut old = query();
    old.remember(&[source("one", 1)]).unwrap();
    old.add_page(token("next"), PageAnchor::Relevance { next_offset: 1 })
        .unwrap();
    let mut invalid = Vec::new();
    let mut next = old.clone();
    next.snapshot.candidates.reverse();
    invalid.push(next);
    let mut next = old.clone();
    next.seen_sources.clear();
    invalid.push(next);
    let mut next = old.clone();
    next.snapshot.continuations.clear();
    invalid.push(next);
    let mut next = old.clone();
    next.snapshot.continuations[0].anchor = PageAnchor::Relevance { next_offset: 0 };
    invalid.push(next);
    let mut next = old.clone();
    next.snapshot.expires_at_unix_ms += 1;
    invalid.push(next);
    let mut next = old.clone();
    next.refreshed_at_unix_ms += 1;
    invalid.push(next);
    let mut next = old.clone();
    next.source_observation_generation -= 1;
    invalid.push(next);
    let mut next = old.clone();
    next.source_observed_at_unix_ms += 1;
    invalid.push(next);
    let mut next = old.clone();
    next.snapshot.device_refs.push("device-C".into());
    invalid.push(next);
    let mut next = old.clone();
    next.snapshot.control_generation += 1;
    invalid.push(next);
    for next in invalid {
        assert!(old.validate_successor(&next).is_err());
    }
}

#[test]
fn observation_time_cannot_precede_admission_outlive_the_query_or_follow_its_last_refresh() {
    let original = query();
    assert_eq!(original.snapshot.captured_at_unix_ms, 1000);
    assert_eq!(original.source_observed_at_unix_ms, 1001);
    for observed in [
        999,
        original.refreshed_at_unix_ms + 1,
        original.snapshot.expires_at_unix_ms,
    ] {
        let mut changed = original.clone();
        changed.source_observed_at_unix_ms = observed;
        assert!(changed.validate().is_err());
        assert!(FrozenRetrievalQuery::decode(&serde_json::to_string(&changed).unwrap()).is_err());
    }
}

#[test]
fn emitted_versions_and_cursor_identity_conflicts_fail_atomically() {
    let mut query = query();
    query
        .remember(&[source("one", 1), source("one", 1)])
        .unwrap();
    assert_eq!(query.seen_sources.len(), 1);
    let cursor = query
        .add_page(token("next"), PageAnchor::Relevance { next_offset: 1 })
        .unwrap();
    let before = query.clone();
    assert!(
        query
            .add_page(token("next"), PageAnchor::Relevance { next_offset: 2 })
            .is_err()
    );
    assert_eq!(query, before);
    assert_eq!(
        query
            .add_page(
                token("duplicate-position"),
                PageAnchor::Relevance { next_offset: 1 }
            )
            .unwrap(),
        cursor
    );
    assert!(
        query
            .add_page(
                "not-a-page-id".into(),
                PageAnchor::Relevance { next_offset: 1 }
            )
            .is_err()
    );
}

#[test]
fn time_queries_keep_the_original_range_and_reject_invalid_keyset_times() {
    let mut time = request();
    if let FrozenRetrievalRequest::Conversations { request } = &mut time {
        request.sort = HistorySort::Time;
        request.queries.clear();
        request.time_range = Some(HistoryTimeRange {
            from_unix_ms: 100,
            until_unix_ms: 1000,
        });
    }
    let mut query = FrozenRetrievalQuery::prepare(
        binding(),
        &reservation(RetrievalCallKind::TimeEnumeration),
        time,
        selection(),
        1100,
        desk_agent_protocol::ai_assistant::limits::RetrievalEpisodeLimits {
            query_ttl_ms: desk_agent_protocol::ai_assistant::limits::DEFAULT_QUERY_TTL_MS,
            ..Default::default()
        },
    )
    .unwrap();
    let before = query.clone();
    for time in [0, i64::MAX as u64 + 1] {
        assert!(
            query
                .add_page(
                    token("bad-time"),
                    PageAnchor::Time {
                        time_unix_ms: time,
                        history_id: 1
                    }
                )
                .is_err()
        );
        assert_eq!(query, before);
    }
    assert!(
        query
            .add_page(
                token("time"),
                PageAnchor::Time {
                    time_unix_ms: 900,
                    history_id: 1
                }
            )
            .is_ok()
    );
    let cursor = query
        .add_page(
            token("next-time"),
            PageAnchor::Time {
                time_unix_ms: 800,
                history_id: 2,
            },
        )
        .unwrap();
    let mut reversed = query.request.clone();
    if let FrozenRetrievalRequest::Conversations { request } = &mut reversed {
        request.order = desk_agent_protocol::ai_assistant::history::HistoryOrder::Asc;
    }
    assert!(query.page(&cursor, &binding(), &reversed, 1200).is_err());
}

#[test]
fn persisted_query_is_strict_bounded_and_binds_its_full_original_metadata() {
    let query = query();
    let raw = query.encode().unwrap();
    assert_eq!(FrozenRetrievalQuery::decode(&raw).unwrap(), query);
    assert_eq!(
        query.fingerprint().unwrap(),
        format!("{:x}", Sha256::digest(raw.as_bytes()))
    );
    let mut altered = serde_json::to_value(&query).unwrap();
    altered["unknown"] = serde_json::json!(true);
    assert!(FrozenRetrievalQuery::decode(&altered.to_string()).is_err());
    assert!(FrozenRetrievalQuery::decode(&" ".repeat(1024 * 1024 + 1)).is_err());
    let mut too_large = query;
    too_large.snapshot.device_refs = (0..10_000)
        .map(|index| format!("device-{index:05}-{}", "a".repeat(120)))
        .collect();
    assert!(too_large.validate_shape().is_ok());
    assert!(too_large.validate().is_err());
    assert!(too_large.encode().is_err());
}

#[test]
fn tool_binding_uses_the_admitted_session_scope() {
    use crate::{
        ai_assistant::{seam::AssistantDataCall, test_support::personal_session},
        chat::ToolCall,
    };
    let mut session = personal_session("actor", "root", "device-A");
    session
        .assistant_data
        .as_mut()
        .unwrap()
        .memory_access_scope
        .as_mut()
        .unwrap()
        .device_rule = AssistantDeviceScope::CurrentDevice;
    let call = ToolCall {
        id: "tool-call".into(),
        name: super::super::data_tools::SEARCH_MEMORIES.into(),
        arguments_json: "{\"queries\":[\"environment\"]}".into(),
    };
    let data = session.assistant_data.as_mut().unwrap();
    let logical = native_ids::stable(
        Kind::AssistantReadAuthorization,
        &(
            &session.actor_id,
            &session.conversation_id,
            &data.retrieval_budget.episode_id,
            "producer",
            &call.id,
        ),
    );
    data.retrieval_budget
        .reserve(
            &logical,
            &super::super::read_projection::request_sha256(&call.name, &call.arguments_json)
                .unwrap(),
            RetrievalCallKind::Relevance,
            16384,
            1000,
        )
        .unwrap();
    let destination = destination("model");
    // This label is an inert fixture. Native producer/output authorization is
    // owned by the host transaction, beyond the scope of this binding builder.
    let parent = crate::model_message_labels::model_bound_user_message(
        "parent".into(),
        "parent".into(),
        destination.clone(),
    )
    .unwrap()
    .data_envelope
    .unwrap();
    session.conversation.push(crate::chat::ChatMessage::text(
        "producer",
        crate::chat::ChatRole::Assistant,
        "inert binding fixture",
    ));
    fn context<'a>(
        session: &'a crate::session::PersistedAgentSession,
        call: &'a ToolCall,
        arguments: &'a super::super::data_tools::ParsedDataCall,
        logical: &'a str,
        parent: &'a desk_agent_protocol::data_lineage::DataEnvelope,
        destination: &'a DestinationIdentity,
    ) -> AssistantDataCall<'a> {
        AssistantDataCall {
            session,
            tool_call: call,
            arguments,
            producer_message: session
                .conversation
                .iter()
                .find(|message| message.message_id == "producer")
                .unwrap(),
            logical_call_id: logical,
            retrieval_reservation: Some(
                &session
                    .assistant_data
                    .as_ref()
                    .unwrap()
                    .retrieval_budget
                    .reservations[0],
            ),
            producer_output_envelope: parent,
            maximum_response_bytes: 16384,
            destination,
            now_unix_ms: 1100,
        }
    }
    let arguments = super::super::data_tools::parse(&call).unwrap();
    let original = context(&session, &call, &arguments, &logical, &parent, &destination);
    let binding =
        RetrievalQueryBinding::for_call(&original, SourceReadPurpose::ModelMemory).unwrap();
    assert_eq!(
        binding.memory_device_rule,
        Some(AssistantDeviceScope::CurrentDevice)
    );
    assert!(binding.history_scope.is_none());
    assert_eq!(
        binding.scope,
        super::super::data_access::assistant_scope(&session).unwrap()
    );
    assert!(RetrievalQueryBinding::for_call(&original, SourceReadPurpose::ModelHistory).is_err());
    session.assistant_data.as_mut().unwrap().memory_access_scope = None;
    assert!(
        RetrievalQueryBinding::for_call(
            &context(&session, &call, &arguments, &logical, &parent, &destination),
            SourceReadPurpose::ModelMemory
        )
        .is_err()
    );
}

#[test]
fn delivered_call_ledger_is_bounded_idempotent_and_preserved_when_sources_and_pages_refresh() {
    let mut original = query();
    assert!(original.mark_delivered("logical-z").unwrap());
    assert!(original.mark_delivered("logical-a").unwrap());
    assert!(!original.mark_delivered("logical-z").unwrap());
    assert!(original.was_delivered("logical-a"));
    assert_eq!(
        original.delivered_logical_call_ids,
        vec!["logical-a", "logical-z"]
    );
    let mut refreshed = original.clone();
    let mut current = selection();
    current.watermark.corpus_generation += 1;
    refreshed.refresh(current, 1100).unwrap();
    original.validate_successor(&refreshed).unwrap();
    assert_eq!(
        FrozenRetrievalQuery::decode(&refreshed.encode().unwrap()).unwrap(),
        refreshed
    );
    refreshed.delivered_logical_call_ids.clear();
    assert!(original.validate_successor(&refreshed).is_err());
    let mut bounded = query();
    for index in 0..bounded.snapshot.limits.query_delivered_calls {
        bounded
            .mark_delivered(&format!("logical-{index:03}"))
            .unwrap();
    }
    let before = bounded.clone();
    assert!(bounded.mark_delivered("one-too-many").is_err());
    assert_eq!(bounded, before);
    assert!(bounded.mark_delivered("").is_err());
    assert_eq!(bounded, before);
}

#[test]
fn configured_query_lifetime_uses_original_admission_and_accepts_longer_snapshots() {
    for ttl in [60_000, 3_600_000] {
        let original = reservation(RetrievalCallKind::Relevance);
        let make = |now| {
            FrozenRetrievalQuery::prepare(
                binding(),
                &original,
                request(),
                selection(),
                now,
                desk_agent_protocol::ai_assistant::limits::RetrievalEpisodeLimits {
                    query_ttl_ms: ttl,
                    ..Default::default()
                },
            )
        };
        let first = make(1100).unwrap();
        let recovered = make(20_000).unwrap();
        assert_eq!(first.snapshot.expires_at_unix_ms, 1000 + ttl);
        assert_eq!(
            first.snapshot.expires_at_unix_ms,
            recovered.snapshot.expires_at_unix_ms
        );
        let decoded: FrozenRetrievalQuery =
            serde_json::from_str(&serde_json::to_string(&recovered).unwrap()).unwrap();
        decoded.validate().unwrap();
        assert!(make(1000 + ttl).is_err());
        let mut task = binding();
        task.authority_expires_at_unix_ms = Some(1500);
        let bounded = FrozenRetrievalQuery::prepare(
            task,
            &original,
            request(),
            selection(),
            1100,
            desk_agent_protocol::ai_assistant::limits::RetrievalEpisodeLimits {
                query_ttl_ms: ttl,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(bounded.snapshot.expires_at_unix_ms, 1500);
    }
}

#[test]
fn query_capacities_are_frozen_and_custom_state_bounds_apply() {
    let mut limits = desk_agent_protocol::ai_assistant::limits::RetrievalEpisodeLimits {
        query_devices: 1,
        ..Default::default()
    };
    assert!(
        FrozenRetrievalQuery::prepare(
            binding(),
            &reservation(RetrievalCallKind::Relevance),
            request(),
            selection(),
            1001,
            limits
        )
        .is_err()
    );
    limits.query_devices = 3;
    limits.query_cursors = 40;
    limits.query_delivered_calls = 40;
    let mut query = FrozenRetrievalQuery::prepare(
        binding(),
        &reservation(RetrievalCallKind::Relevance),
        request(),
        selection(),
        1001,
        limits,
    )
    .unwrap();
    for index in 0..40 {
        query.mark_delivered(&format!("logical-{index}")).unwrap();
    }
    assert!(query.mark_delivered("excess").is_err());
    let encoded = query.encode().unwrap();
    limits.query_delivered_calls = 1;
    assert_eq!(limits.query_delivered_calls, 1);
    assert_eq!(
        FrozenRetrievalQuery::decode(&encoded)
            .unwrap()
            .snapshot
            .limits
            .query_delivered_calls,
        40
    );
    query.snapshot.limits.query_state_bytes = (encoded.len() / 2) as u32;
    assert!(query.encode().is_err());
}

#[test]
fn an_existing_query_cannot_raise_or_lower_its_frozen_capacity_on_commit() {
    let original = query();
    let mut changed = original.clone();
    changed.snapshot.limits.query_seen_sources += 1;
    assert!(original.validate_successor(&changed).is_err());
}

#[test]
fn configured_query_capacity_accepts_more_than_ten_thousand_devices_and_one_mibibyte() {
    let mut selected = selection();
    selected.devices = (0..15000)
        .map(|index| format!("device-{index:05}-{}", "a".repeat(80)))
        .collect();
    let limits = desk_agent_protocol::ai_assistant::limits::RetrievalEpisodeLimits {
        query_devices: 20000,
        query_state_bytes: 2 * 1024 * 1024,
        ..Default::default()
    };
    let mut frozen = FrozenRetrievalQuery::prepare(
        binding(),
        &reservation(RetrievalCallKind::Relevance),
        request(),
        selected,
        1001,
        limits,
    )
    .unwrap();
    let encoded = frozen.encode().unwrap();
    assert!(encoded.len() > 1024 * 1024);
    assert_eq!(FrozenRetrievalQuery::decode(&encoded).unwrap(), frozen);
    frozen.snapshot.limits.query_state_bytes = 1024 * 1024;
    assert!(frozen.encode().is_err());
}

#[test]
fn memory_history_refresh_keeps_its_original_head_bound_and_uses_revision_pages() {
    let request = FrozenRetrievalRequest::MemoryHistory {
        request: MemoryHistoryRequest {
            memory_id: "memory".into(),
            limit: 2,
            cursor: None,
        },
        current_revision: 3,
    };
    let mut original = FrozenRetrievalQuery::prepare(
        binding(),
        &reservation(RetrievalCallKind::Read),
        request.clone(),
        selection(),
        1001,
        Default::default(),
    )
    .unwrap();
    let cursor = original
        .add_page(
            token("old-versions"),
            PageAnchor::MemoryRevision { before_revision: 2 },
        )
        .unwrap();
    assert_eq!(original.snapshot.purpose, SourceReadPurpose::ModelMemory);
    assert!(
        original
            .add_page(
                token("bad-bound"),
                PageAnchor::MemoryRevision { before_revision: 4 }
            )
            .is_err()
    );
    let mut refreshed = original.clone();
    let mut changed = selection();
    changed.watermark.memory_generation += 1;
    refreshed.refresh(changed, 1100).unwrap();
    assert_eq!(refreshed.request, request);
    original.validate_successor(&refreshed).unwrap();
    assert_eq!(
        refreshed
            .page(&cursor, &refreshed.binding, &request, 1101)
            .unwrap(),
        &PageAnchor::Start
    );
    if let FrozenRetrievalRequest::MemoryHistory {
        current_revision, ..
    } = &mut refreshed.request
    {
        *current_revision = 4;
    }
    assert!(original.validate_successor(&refreshed).is_err());
}

#[test]
fn account_conversations_may_query_every_owner_device() {
    // A personal memory query with request targets may still widen to all devices.
    let mut memory = binding();
    memory.source_device_ref.clear();
    memory.history_scope = None;
    memory.request_memory_device_refs = vec!["device-A".into()];
    let request = FrozenRetrievalRequest::Memories {
        request: serde_json::from_value(
            serde_json::json!({"queries": ["settings"], "scope": "all_my_devices"}),
        )
        .unwrap(),
    };
    let query = FrozenRetrievalQuery::prepare(
        memory.clone(),
        &reservation(RetrievalCallKind::Relevance),
        request,
        selection(),
        1100,
        Default::default(),
    )
    .unwrap();
    assert_eq!(query.snapshot.device_refs, ["device-A", "device-B"]);
    // History bindings never carry request targets.
    memory.history_scope = binding().history_scope;
    assert!(memory.validate().is_err());

    // Account-level conversations appear as the empty device.
    let mut history = binding();
    history.source_device_ref.clear();
    history
        .history_scope
        .as_mut()
        .unwrap()
        .current_device_ref
        .clear();
    let mut devices = selection();
    devices.devices.push(String::new());
    let query = FrozenRetrievalQuery::prepare(
        history,
        &reservation(RetrievalCallKind::Relevance),
        FrozenRetrievalRequest::Conversations {
            request: serde_json::from_value(
                serde_json::json!({"queries": ["settings"], "scope": "all_my_devices"}),
            )
            .unwrap(),
        },
        devices,
        1100,
        Default::default(),
    )
    .unwrap();
    assert!(query.snapshot.device_refs.contains(&String::new()));
}
