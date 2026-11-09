use super::*;
use crate::{
    ai_assistant::{
        data_tools, history::RetrievalCallKind, retrieval_response::ToolReplyBudget,
        seam::AssistantReadRoot, test_support::personal_session,
    },
    chat::{ChatMessage, ModelTurn, StopReason, ToolCall},
    model_egress::ModelEgressPolicy,
    model_message_labels::model_bound_user_message,
    session::PersistedAgentSession,
};
use desk_agent_protocol::ai_assistant::{
    history::{
        AssistantDeviceScope, AssistantHistoryCoverage, ConversationReadResult,
        ConversationSearchHit, ConversationSearchResult, HistoryMessage,
    },
    source::{AssistantSourceDevice, AssistantSourceKind, AssistantSourceLocator},
    user_instructions::AssistantDataSettings,
};
use desk_agent_protocol::data_lineage::DestinationIdentity;

// These fixtures exercise common bindings. A host must still verify the actual
// accepted native producer and every source in its original database transaction.
struct Fixture {
    session: PersistedAgentSession,
    call: ToolCall,
    arguments: crate::ai_assistant::data_tools::ParsedDataCall,
    logical: String,
    destination: DestinationIdentity,
    parent: DataEnvelope,
    access: AssistantDataToolAccess,
    maximum: u32,
}
impl Fixture {
    fn new(name: &str, arguments: &str, kind: RetrievalCallKind) -> Self {
        let mut session = personal_session("owner", "root", "device");
        session.turn_state = crate::session::TurnState::Running;
        session.input_revision = 1;
        session.lease_token = 1;
        let destination = DestinationIdentity::Model {
            connection_id: "gateway".into(),
            connection_revision: 1,
            model_id: "model".into(),
            profile_revision: 1,
        };
        let call = ToolCall {
            id: "call".into(),
            name: name.into(),
            arguments_json: arguments.into(),
        };
        let policy = ModelEgressPolicy {
            destination: destination.clone(),
            selected_source_tools: Default::default(),
            export_authorization_id: "producer-export".into(),
            now_unix_ms: 900,
            byte_cap: crate::sink_authorizer::MAX_SINK_BYTES,
            permission_resume: false,
        };
        let turn = ModelTurn {
            tool_calls: vec![call.clone()],
            stop_reason: StopReason::ToolUse,
            ..Default::default()
        };
        let input = model_bound_user_message(
            "input".into(),
            "look up a prior report".into(),
            destination.clone(),
        )
        .unwrap()
        .data_envelope
        .unwrap();
        let parent = policy
            .derive_model_output_envelope(&turn, &[input])
            .unwrap();
        let mut producer = ChatMessage::assistant_tool_calls("producer", "", vec![call.to_ref()]);
        producer.data_envelope = Some(parent.clone());
        session.conversation.push(producer);
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
        let maximum = if kind == RetrievalCallKind::Relevance {
            16384
        } else {
            32768
        };
        data.retrieval_budget
            .reserve(
                &logical,
                &request_sha256(name, arguments).unwrap(),
                kind,
                maximum,
                1000,
            )
            .unwrap();
        let arguments = crate::ai_assistant::data_tools::parse(&call).unwrap();
        Self {
            session,
            call,
            arguments,
            logical,
            destination,
            parent,
            maximum,
            access: AssistantDataToolAccess {
                settings: AssistantDataSettings::default(),
                policy_revision: 1,
                memory_state_generation: 1,
                control_generation: 1,
            },
        }
    }
    fn context(&self) -> AssistantDataCall<'_> {
        AssistantDataCall {
            session: &self.session,
            tool_call: &self.call,
            arguments: &self.arguments,
            producer_message: self
                .session
                .conversation
                .iter()
                .find(|message| message.message_id == "producer")
                .unwrap(),
            logical_call_id: &self.logical,
            retrieval_reservation: Some(
                &self
                    .session
                    .assistant_data
                    .as_ref()
                    .unwrap()
                    .retrieval_budget
                    .reservations[0],
            ),
            producer_output_envelope: &self.parent,
            maximum_response_bytes: self.maximum,
            destination: &self.destination,
            now_unix_ms: 1100,
        }
    }
    fn usage(&self, name: &str) -> AssistantDataReadUse {
        let source = native_ids::fixture_source_id(&name);
        AssistantDataReadUse {
            decision_roots: Vec::new(),
            projection_id: native_ids::stable(Kind::AssistantReadProjection, &name),
            source_envelope_id: "candidate-only".into(),
            purpose: SourceReadPurpose::ModelHistory,
            retrieval_episode_id: self
                .session
                .assistant_data
                .as_ref()
                .unwrap()
                .retrieval_budget
                .episode_id
                .clone(),
            scope: assistant_scope(&self.session).unwrap(),
            policy_revision: 1,
            control_generation: 1,
            memory_state_generation: 1,
            roots: vec![AssistantReadRoot {
                source_id: source.clone(),
                source_revision: 1,
            }],
            history_dependencies: vec![HistoryDependency {
                source_id: source,
                source_revision: 1,
                origin_id: native_ids::stable(Kind::AssistantOrigin, &name),
                origin_deletion_generation: 1,
                policy_revision: 1,
                expires_at_unix_ms: Some(10_000),
            }],
            memories: Vec::new(),
            expires_at_unix_ms: Some(10_000),
        }
    }
}
// Exercise stored-data validation rather than revalidating an internal constructor.
fn persisted_read(
    context: &AssistantDataCall<'_>,
    access: &AssistantDataToolAccess,
    content: &str,
    purpose: SourceReadPurpose,
    selected: &[AssistantDataReadUse],
    now: u64,
) -> Result<ReadProjectionProof, &'static str> {
    let context = &AssistantDataCall {
        now_unix_ms: now,
        ..*context
    };
    let original =
        crate::ai_assistant::assistant_data_admission::OriginalAssistantDataCall::retrieval(
            context, now,
        )?;
    original.persisted_producer(context, context.session, now)?;
    let proof =
        ReadProjectionProof::for_retrieval(context, access, content, purpose, selected, now)?;
    let stored = serde_json::to_string(&proof).unwrap();
    let decoded = ReadProjectionProof::decode_snapshot(&stored, Some(content))?;
    decoded.validate_retrieval_replay(context, access, content)?;
    Ok(*decoded)
}

fn device() -> AssistantSourceDevice {
    AssistantSourceDevice {
        device_ref: "device".into(),
        recorded_name: None,
        current_name: None,
        historical: false,
    }
}
fn search(sources: &[AssistantDataReadUse]) -> String {
    serde_json::to_string(&ConversationSearchResult {
        hits: sources
            .iter()
            .map(|usage| {
                let root = &usage.roots[0];
                ConversationSearchHit {
                    source_id: root.source_id.clone(),
                    revision: root.source_revision,
                    kind: AssistantSourceKind::Input,
                    locator: AssistantSourceLocator {
                        conversation_id: "past-root".into(),
                        root_conversation_id: "past-root".into(),
                        message_id: Some("old-message".into()),
                        input_event_id: None,
                        action_id: None,
                        result_id: None,
                        fragment_index: 0,
                        byte_start: 0,
                        byte_end: 20,
                    },
                    source_device: device(),
                    conversation_title: None,
                    snippet: "report excerpt".into(),
                    recorded_at_unix_ms: 900,
                    statement_timezone: "UTC".into(),
                    native_action_at_unix_ms: None,
                    native_action_interval: None,
                    native_time_basis: None,
                    native_status_summary: None,
                }
            })
            .collect(),
        scope: AssistantDeviceScope::CurrentDevice,
        effective_queries: vec![vec!["report".into()]],
        retrieval_mode: "relevance".into(),
        has_more: false,
        next_cursor: None,
        scope_changed: false,
        coverage: AssistantHistoryCoverage {
            retained_from_unix_ms: Some(1),
            retained_until_unix_ms: Some(1000),
            indexed_through_unix_ms: Some(1000),
            pending: 0,
            failed: 0,
            has_gaps: false,
            partial: false,
            reasons: Vec::new(),
        },
    })
    .unwrap()
}

#[test]
fn prepared_reads_bind_exact_returned_roots_complete_shared_dependencies_and_the_actual_model() {
    let fixture = Fixture::new(
        data_tools::SEARCH_CONVERSATIONS,
        "{\"queries\":[\"report\"]}",
        RetrievalCallKind::Relevance,
    );
    let mut a = fixture.usage("A");
    let mut b = fixture.usage("B");
    let common = fixture.usage("shared");
    a.history_dependencies
        .extend(common.history_dependencies.clone());
    b.history_dependencies.extend(common.history_dependencies);
    let content = search(&[a.clone(), b.clone()]);
    let proof = persisted_read(
        &fixture.context(),
        &fixture.access,
        &content,
        SourceReadPurpose::ModelHistory,
        &[a, b],
        1100,
    )
    .unwrap();
    assert_eq!(proof.usage.roots.len(), 2);
    assert_eq!(proof.usage.history_dependencies.len(), 3);
    assert_eq!(proof.unused_until_unix_ms, 10_000);
    assert_eq!(
        proof.source_envelope.allowed_destinations,
        std::slice::from_ref(&fixture.destination)
    );
    assert!(!proof.source_envelope.retention.delete_with_run);
    assert_eq!(
        proof.source_envelope.provenance.source_envelope_ids.len(),
        3
    );
    proof.validate_cached_result(&content).unwrap();
    proof
        .validate_retrieval_replay(
            &AssistantDataCall {
                now_unix_ms: 1200,
                ..fixture.context()
            },
            &fixture.access,
            &content,
        )
        .unwrap();
}

#[test]
fn source_purpose_and_preparation_deadlines_are_never_renewed_from_a_retry_clock() {
    let mut fixture = Fixture::new(
        data_tools::SEARCH_CONVERSATIONS,
        "{\"queries\":[\"report\"]}",
        RetrievalCallKind::Relevance,
    );
    let mut usage = fixture.usage("A");
    usage.expires_at_unix_ms = None;
    usage.history_dependencies[0].expires_at_unix_ms = None;
    let content = search(std::slice::from_ref(&usage));
    let first = persisted_read(
        &fixture.context(),
        &fixture.access,
        &content,
        SourceReadPurpose::ModelHistory,
        std::slice::from_ref(&usage),
        1100,
    )
    .unwrap();
    let later = persisted_read(
        &fixture.context(),
        &fixture.access,
        &content,
        SourceReadPurpose::ModelHistory,
        std::slice::from_ref(&usage),
        20_000,
    )
    .unwrap();
    assert_eq!(
        first.unused_until_unix_ms,
        1000 + desk_agent_protocol::ai_assistant::limits::DEFAULT_UNUSED_PROJECTION_TTL_MS
    );
    assert_eq!(first.unused_until_unix_ms, later.unused_until_unix_ms);
    assert!(
        persisted_read(
            &fixture.context(),
            &fixture.access,
            &content,
            SourceReadPurpose::ModelHistory,
            &[usage],
            first.unused_until_unix_ms
        )
        .is_err()
    );
    fixture
        .session
        .assistant_data
        .as_mut()
        .unwrap()
        .history_access_scope
        .as_mut()
        .unwrap()
        .expires_at_unix_ms = Some(1400);
    let usage = fixture.usage("B");
    let content = search(std::slice::from_ref(&usage));
    let narrowed = persisted_read(
        &fixture.context(),
        &fixture.access,
        &content,
        SourceReadPurpose::ModelHistory,
        &[usage],
        1100,
    )
    .unwrap();
    assert_eq!(narrowed.unused_until_unix_ms, 1400);
    assert_eq!(
        narrowed.source_envelope.retention.expires_at_unix_ms,
        Some(10_000)
    );
}

#[test]
fn mismatched_arguments_accounting_classes_producers_and_destinations_cannot_prepare_a_read() {
    let mut fixture = Fixture::new(
        data_tools::SEARCH_CONVERSATIONS,
        "{\"queries\":[\"report\"]}",
        RetrievalCallKind::Read,
    );
    let usage = fixture.usage("A");
    let content = search(std::slice::from_ref(&usage));
    assert!(
        persisted_read(
            &fixture.context(),
            &fixture.access,
            &content,
            SourceReadPurpose::ModelHistory,
            &[usage],
            1100
        )
        .is_err()
    );
    fixture = Fixture::new(
        data_tools::SEARCH_CONVERSATIONS,
        "{\"queries\":[\"report\"]}",
        RetrievalCallKind::Relevance,
    );
    let usage = fixture.usage("A");
    let content = search(std::slice::from_ref(&usage));
    fixture
        .session
        .conversation
        .push(fixture.session.conversation[0].clone());
    assert!(
        persisted_read(
            &fixture.context(),
            &fixture.access,
            &content,
            SourceReadPurpose::ModelHistory,
            std::slice::from_ref(&usage),
            1100
        )
        .is_err()
    );
    fixture.session.conversation.pop();
    fixture.destination = DestinationIdentity::Model {
        connection_id: "other".into(),
        connection_revision: 1,
        model_id: "model".into(),
        profile_revision: 1,
    };
    assert!(
        persisted_read(
            &fixture.context(),
            &fixture.access,
            &content,
            SourceReadPurpose::ModelHistory,
            &[usage],
            1100
        )
        .is_err()
    );
}

#[test]
fn result_ids_hashes_and_actual_output_roots_cannot_be_fabricated_or_padded_with_omitted_sources() {
    let fixture = Fixture::new(
        data_tools::SEARCH_CONVERSATIONS,
        "{\"queries\":[\"report\"]}",
        RetrievalCallKind::Relevance,
    );
    let a = fixture.usage("A");
    let b = fixture.usage("B");
    let content = search(std::slice::from_ref(&a));
    assert!(
        persisted_read(
            &fixture.context(),
            &fixture.access,
            &content,
            SourceReadPurpose::ModelHistory,
            &[a.clone(), b.clone()],
            1100
        )
        .is_err()
    );
    assert!(
        persisted_read(
            &fixture.context(),
            &fixture.access,
            &search(&[b]),
            SourceReadPurpose::ModelHistory,
            std::slice::from_ref(&a),
            1100
        )
        .is_err()
    );
}

#[test]
fn dependency_conflicts_and_current_control_or_feature_changes_fail_closed() {
    let mut fixture = Fixture::new(
        data_tools::SEARCH_CONVERSATIONS,
        "{\"queries\":[\"report\"]}",
        RetrievalCallKind::Relevance,
    );
    let mut a = fixture.usage("A");
    let mut b = fixture.usage("B");
    let shared = fixture.usage("shared");
    a.history_dependencies
        .extend(shared.history_dependencies.clone());
    b.history_dependencies.extend(shared.history_dependencies);
    b.history_dependencies[1].origin_deletion_generation += 1;
    let content = search(&[a.clone(), b.clone()]);
    assert!(
        persisted_read(
            &fixture.context(),
            &fixture.access,
            &content,
            SourceReadPurpose::ModelHistory,
            &[a.clone(), b],
            1100
        )
        .is_err()
    );
    fixture.access.settings.model_history_search_enabled = false;
    assert!(
        persisted_read(
            &fixture.context(),
            &fixture.access,
            &search(std::slice::from_ref(&a)),
            SourceReadPurpose::ModelHistory,
            std::slice::from_ref(&a),
            1100
        )
        .is_err()
    );
    fixture.access.settings.model_history_search_enabled = true;
    fixture.access.control_generation += 1;
    assert!(
        persisted_read(
            &fixture.context(),
            &fixture.access,
            &search(std::slice::from_ref(&a)),
            SourceReadPurpose::ModelHistory,
            &[a],
            1100
        )
        .is_err()
    );
}

#[test]
fn original_window_content_and_self_reported_bytes_must_match_the_retained_full_fragment() {
    let source = native_ids::fixture_source_id(&"A");
    let arguments = serde_json::json!({"source_id":source,"max_bytes":32768}).to_string();
    let fixture = Fixture::new(
        data_tools::READ_CONVERSATION,
        &arguments,
        RetrievalCallKind::Read,
    );
    let body = "exact 汉 fragment";
    let mut usage = fixture.usage("A");
    usage.decision_roots = usage.roots.clone();
    let mut result = ConversationReadResult {
        conversation_id: "past-root".into(),
        messages: vec![HistoryMessage {
            source_id: source,
            message_id: "old-message".into(),
            revision: 1,
            role: "user".into(),
            content: body.into(),
            recorded_at_unix_ms: 900,
            statement_timezone: "UTC".into(),
            source_device: device(),
        }],
        related_results: Vec::new(),
        previous_cursor: None,
        next_cursor: None,
        source_observed_through_unix_ms: 1000,
        actual_bytes: 0,
        has_more: false,
        partial: false,
        reasons: Vec::new(),
    };
    let budget = ToolReplyBudget::new("call", 32768).unwrap();
    let framed = budget.try_encode_window(&mut result).unwrap().unwrap();
    persisted_read(
        &fixture.context(),
        &fixture.access,
        &framed.content,
        SourceReadPurpose::ModelHistory,
        std::slice::from_ref(&usage),
        1100,
    )
    .unwrap();
    result.actual_bytes = 1;
    assert!(
        persisted_read(
            &fixture.context(),
            &fixture.access,
            &serde_json::to_string(&result).unwrap(),
            SourceReadPurpose::ModelHistory,
            &[usage],
            1100
        )
        .is_err()
    );
}

#[test]
fn a_window_keeps_its_original_anchor_as_a_constraint_without_claiming_its_body_was_returned() {
    let source = native_ids::fixture_source_id(&"anchor");
    let arguments =
        serde_json::json!({"source_id":source,"view":"latest","max_bytes":32768}).to_string();
    let fixture = Fixture::new(
        data_tools::READ_CONVERSATION,
        &arguments,
        RetrievalCallKind::Read,
    );
    let mut anchor = fixture.usage("anchor");
    anchor.decision_roots = std::mem::take(&mut anchor.roots);
    let tail = fixture.usage("tail");
    let mut result = ConversationReadResult {
        conversation_id: "past-root".into(),
        messages: vec![HistoryMessage {
            source_id: tail.roots[0].source_id.clone(),
            message_id: "tail-message".into(),
            revision: 1,
            role: "assistant".into(),
            content: "actual returned tail".into(),
            recorded_at_unix_ms: 950,
            statement_timezone: "UTC".into(),
            source_device: device(),
        }],
        related_results: Vec::new(),
        previous_cursor: None,
        next_cursor: None,
        source_observed_through_unix_ms: 1000,
        actual_bytes: 0,
        has_more: false,
        partial: false,
        reasons: Vec::new(),
    };
    let budget = ToolReplyBudget::new("call", 32768).unwrap();
    let content = budget
        .try_encode_window(&mut result)
        .unwrap()
        .unwrap()
        .content;
    let proof = persisted_read(
        &fixture.context(),
        &fixture.access,
        &content,
        SourceReadPurpose::ModelHistory,
        &[tail.clone(), anchor.clone()],
        1100,
    )
    .unwrap();
    assert_eq!(proof.usage.roots, tail.roots);
    assert_eq!(proof.usage.decision_roots, anchor.decision_roots);
    assert_eq!(proof.usage.history_dependencies.len(), 2);
    assert!(
        !proof
            .usage
            .roots
            .iter()
            .any(|root| root.source_id == source)
    );

    // Empty output still binds the exact original anchor; it never becomes an
    // unconstrained successful read after filtering all returned fragments.
    result.messages.clear();
    let empty = budget
        .try_encode_window(&mut result)
        .unwrap()
        .unwrap()
        .content;
    let empty_proof = persisted_read(
        &fixture.context(),
        &fixture.access,
        &empty,
        SourceReadPurpose::ModelHistory,
        std::slice::from_ref(&anchor),
        1100,
    )
    .unwrap();
    assert!(empty_proof.usage.roots.is_empty());
    assert_eq!(empty_proof.usage.decision_roots, anchor.decision_roots);
    assert!(
        persisted_read(
            &fixture.context(),
            &fixture.access,
            &empty,
            SourceReadPurpose::ModelHistory,
            &[],
            1100
        )
        .is_err()
    );
    assert!(
        persisted_read(
            &fixture.context(),
            &fixture.access,
            &content,
            SourceReadPurpose::ModelHistory,
            std::slice::from_ref(&tail),
            1100
        )
        .is_err()
    );
    let mut wrong_anchor = tail.clone();
    wrong_anchor.decision_roots = std::mem::take(&mut wrong_anchor.roots);
    assert!(
        persisted_read(
            &fixture.context(),
            &fixture.access,
            &content,
            SourceReadPurpose::ModelHistory,
            &[tail.clone(), wrong_anchor],
            1100
        )
        .is_err()
    );

    let mut padded = anchor.clone();
    padded.roots = padded.decision_roots.clone();
    assert!(
        persisted_read(
            &fixture.context(),
            &fixture.access,
            &content,
            SourceReadPurpose::ModelHistory,
            &[tail, padded],
            1100
        )
        .is_err()
    );
    let mut relabeled = proof;
    relabeled.tool_name = data_tools::SEARCH_CONVERSATIONS.into();
    assert!(relabeled.validate().is_err());
}

#[test]
fn search_results_cannot_add_a_hidden_window_decision_anchor() {
    let fixture = Fixture::new(
        data_tools::SEARCH_CONVERSATIONS,
        "{\"queries\":[\"report\"]}",
        RetrievalCallKind::Relevance,
    );
    let mut usage = fixture.usage("A");
    let body = search(std::slice::from_ref(&usage));
    usage.decision_roots = usage.roots.clone();
    assert!(
        persisted_read(
            &fixture.context(),
            &fixture.access,
            &body,
            SourceReadPurpose::ModelHistory,
            &[usage],
            1100
        )
        .is_err()
    );
}

#[test]
fn an_empty_native_inventory_names_its_exact_source_and_cannot_hide_extra_read_roots() {
    use crate::ai_assistant::native_history::NativeHistorySnapshot;
    let fixture = Fixture::new(
        data_tools::SEARCH_CONVERSATIONS,
        "{\"queries\":[\"report\"]}",
        RetrievalCallKind::Relevance,
    );
    let input = fixture.usage("input");
    let native = fixture.usage("native");
    let mut result: ConversationSearchResult =
        serde_json::from_str(&search(std::slice::from_ref(&input))).unwrap();
    let root = &native.roots[0];
    result.hits[0].native_status_summary = Some(
        NativeHistorySnapshot::bounded(
            Vec::new(),
            true,
            900,
            desk_agent_protocol::ai_assistant::limits::AssistantProcessingLimits::default()
                .native_status_items as usize,
        )
        .unwrap()
        .summary(
            &root.source_id,
            root.source_revision,
            desk_agent_protocol::ai_assistant::limits::AssistantProcessingLimits::default()
                .native_status_model_items as usize,
        )
        .unwrap(),
    );
    let content = serde_json::to_string(&result).unwrap();
    let proof = persisted_read(
        &fixture.context(),
        &fixture.access,
        &content,
        SourceReadPurpose::ModelHistory,
        &[input.clone(), native.clone()],
        1100,
    )
    .unwrap();
    assert_eq!(proof.usage.roots.len(), 2);
    proof.validate_cached_result(&content).unwrap();
    assert!(
        persisted_read(
            &fixture.context(),
            &fixture.access,
            &content,
            SourceReadPurpose::ModelHistory,
            std::slice::from_ref(&input),
            1100
        )
        .is_err()
    );
    result.hits[0]
        .native_status_summary
        .as_mut()
        .unwrap()
        .source_revision += 1;
    assert!(
        persisted_read(
            &fixture.context(),
            &fixture.access,
            &serde_json::to_string(&result).unwrap(),
            SourceReadPurpose::ModelHistory,
            &[input, native],
            1100
        )
        .is_err()
    );
}

#[test]
fn retained_native_state_families_bind_each_original_observation_and_reject_unlisted_state_sources()
{
    use crate::ai_assistant::native_history::{
        NativeHistorySnapshot, NativeHistoryStatus, NativeHistoryTaskKind,
    };
    use desk_agent_protocol::ai_assistant::source::{AssistantSourceRef, SourceRefRole};
    let fixture = Fixture::new(
        data_tools::SEARCH_CONVERSATIONS,
        "{\"queries\":[\"report\"]}",
        RetrievalCallKind::Relevance,
    );
    let input = fixture.usage("input");
    let current = fixture.usage("current-native");
    let retained = fixture.usage("old-native");
    let mut result: ConversationSearchResult =
        serde_json::from_str(&search(std::slice::from_ref(&input))).unwrap();
    let mut summary = NativeHistorySnapshot::bounded(
        Vec::new(),
        true,
        900,
        desk_agent_protocol::ai_assistant::limits::AssistantProcessingLimits::default()
            .native_status_items as usize,
    )
    .unwrap()
    .summary(
        &current.roots[0].source_id,
        current.roots[0].source_revision,
        desk_agent_protocol::ai_assistant::limits::AssistantProcessingLimits::default()
            .native_status_model_items as usize,
    )
    .unwrap();
    summary.no_native_task = false;
    summary.additional_sources = vec![AssistantSourceRef {
        kind: AssistantSourceKind::NativeStatus,
        id: retained.roots[0].source_id.clone(),
        revision: Some(retained.roots[0].source_revision),
        role: SourceRefRole::Evidence,
    }];
    summary.items = vec![
        NativeHistoryStatus {
            native_id: "old-goal".into(),
            kind: NativeHistoryTaskKind::Goal,
            state: "completed".into(),
            revision: Some(7),
            observed_at_unix_ms: 800,
        }
        .projection(&retained.roots[0].source_id)
        .unwrap(),
    ];
    result.hits[0].native_status_summary = Some(summary);
    let content = serde_json::to_string(&result).unwrap();
    persisted_read(
        &fixture.context(),
        &fixture.access,
        &content,
        SourceReadPurpose::ModelHistory,
        &[input.clone(), current.clone(), retained.clone()],
        1100,
    )
    .unwrap()
    .validate_cached_result(&content)
    .unwrap();
    result.hits[0]
        .native_status_summary
        .as_mut()
        .unwrap()
        .additional_sources
        .clear();
    assert!(
        persisted_read(
            &fixture.context(),
            &fixture.access,
            &serde_json::to_string(&result).unwrap(),
            SourceReadPurpose::ModelHistory,
            &[input, current, retained],
            1100
        )
        .is_err()
    );
}

#[test]
fn configured_unused_lifetime_is_frozen_and_consumption_retains_original_source_lifetime() {
    for ttl in [60_000, 3_600_000] {
        let mut fixture = Fixture::new(
            data_tools::SEARCH_CONVERSATIONS,
            "{\"queries\":[\"report\"]}",
            RetrievalCallKind::Relevance,
        );
        let data = fixture.session.assistant_data.as_mut().unwrap();
        data.retrieval_budget.limits.unused_projection_ttl_ms = ttl;
        data.limits.retrieval.episode.unused_projection_ttl_ms = 1;
        let mut usage = fixture.usage("configured");
        usage.expires_at_unix_ms = None;
        usage.history_dependencies[0].expires_at_unix_ms = None;
        let content = search(std::slice::from_ref(&usage));
        let make = |now| {
            persisted_read(
                &fixture.context(),
                &fixture.access,
                &content,
                SourceReadPurpose::ModelHistory,
                std::slice::from_ref(&usage),
                now,
            )
        };
        let first = make(1100).unwrap();
        let later = make(20_000).unwrap();
        assert_eq!(first.unused_until_unix_ms, 1000 + ttl);
        assert_eq!(first.unused_until_unix_ms, later.unused_until_unix_ms);
        let decoded: ReadProjectionProof =
            serde_json::from_str(&serde_json::to_string(&first).unwrap()).unwrap();
        decoded.validate().unwrap();
        assert!(!decoded.preparation_available(1000 + ttl, None));
        assert!(decoded.preparation_available(1000 + ttl, Some(1100)));
        assert!(make(1000 + ttl).is_err());
    }
}
