use super::*;
use crate::ai_assistant::source::{HistoryCapture, owner_text_policy, prepare_capture};
use desk_agent_protocol::{
    ai_assistant::user_instructions::AssistantDataPolicy, data_lineage::DestinationIdentity,
};
use sha2::{Digest, Sha256};

fn metadata() -> NativeHistoryMetadata {
    NativeHistoryMetadata {
        format_version: 1,
        native_proof_sha256: "a".repeat(64),
        projected_content_sha256: "b".repeat(64),
        projection_partial: false,
        time: Some(NativeHistoryTime::Interval {
            from_unix_ms: 100,
            until_unix_ms: 201,
            basis: NativeTimeBasis::ProviderAckObserved,
        }),
        status_snapshot: None,
    }
}

#[test]
fn source_navigation_uses_only_unique_original_result_envelopes_including_externalized_results() {
    use crate::{
        chat::{ChatMessage, ChatRole},
        conversation_attachment::delivery::RawResult,
    };
    let mut result = crate::model_message_labels::model_bound_user_message(
        "original-native-result".into(),
        "receipt".into(),
        DestinationIdentity::Model {
            connection_id: "gateway".into(),
            connection_revision: 1,
            model_id: "model".into(),
            profile_revision: 1,
        },
    )
    .unwrap();
    let envelope = result.data_envelope.clone().unwrap();
    result.role = ChatRole::UntrustedOutput;
    let mut messages = vec![
        ChatMessage::text("question", ChatRole::User, "question"),
        result.clone(),
    ];
    assert_eq!(
        original_result_message_id(&messages, &envelope.envelope_id).as_deref(),
        Some("original-native-result")
    );
    assert_eq!(
        original_result_message_id(&messages, "different-original-result"),
        None
    );
    messages[1].role = ChatRole::User;
    assert_eq!(
        original_result_message_id(&messages, &envelope.envelope_id),
        None
    );
    result.raw_result = Some(Box::new(RawResult {
        original_sha256: "0".repeat(64),
        original_envelope: Some(envelope.clone()),
        restorable: true,
        template: None,
        slots: Vec::new(),
        envelope: None,
    }));
    result.data_envelope = None;
    messages[1] = result.clone();
    assert_eq!(
        original_result_message_id(&messages, &envelope.envelope_id).as_deref(),
        Some("original-native-result")
    );
    messages.push(result);
    assert_eq!(
        original_result_message_id(&messages, &envelope.envelope_id),
        None
    );
}

#[test]
fn receipt_intervals_are_bound_to_original_metadata_and_cannot_become_exact_send_times() {
    let metadata = metadata();
    let raw = serde_json::to_string(&metadata).unwrap();
    let columns = metadata.columns();
    let read = |value: &str, columns: &NativeHistoryTimeColumns| {
        read_metadata(AssistantSourceKind::Receipt, 200, Some(value), columns)
    };
    assert_eq!(read(&raw, &columns).unwrap(), Some(metadata.clone()));
    for case in 0..4 {
        let mut changed = metadata.clone();
        let mut indexed = columns.clone();
        match case {
            0 => indexed.at_unix_ms = Some(200),
            1 => indexed.from_unix_ms = Some(101),
            2 => indexed.basis = Some("native_recorded".into()),
            _ => {
                changed.time = Some(NativeHistoryTime::Exact {
                    at_unix_ms: 200,
                    basis: NativeTimeBasis::ProviderAckObserved,
                })
            }
        }
        assert!(
            read(&serde_json::to_string(&changed).unwrap(), &indexed).is_err(),
            "case {case}"
        );
    }
    assert!(read_metadata(AssistantSourceKind::Input, 200, Some(&raw), &columns).is_err());
    assert!(
        read_metadata(
            AssistantSourceKind::Receipt,
            200,
            None,
            &NativeHistoryTimeColumns::default()
        )
        .is_err()
    );
}

#[test]
fn original_state_revisions_are_historical_and_not_inferred_from_messages_or_replay_time() {
    let mut metadata = metadata();
    metadata.time = None;
    metadata.status_snapshot = Some(
        NativeHistorySnapshot::bounded(
            vec![NativeHistoryStatus {
                native_id: "work-1".into(),
                kind: NativeHistoryTaskKind::Work,
                state: "outcome_unknown".into(),
                revision: Some(2),
                observed_at_unix_ms: 200,
            }],
            true,
            200,
            desk_agent_protocol::ai_assistant::limits::AssistantProcessingLimits::default()
                .native_status_items as usize,
        )
        .unwrap(),
    );
    metadata
        .validate(AssistantSourceKind::NativeStatus, &"b".repeat(64), 200)
        .unwrap();
    let projection = metadata.status_snapshot.as_ref().unwrap().items[0]
        .projection("original-source")
        .unwrap();
    assert!(projection.historical);
    assert_eq!(projection.revision, Some(2));
    assert_eq!(projection.observed_at_unix_ms, 200);
    assert_eq!(projection.state, "outcome_unknown");
    assert!(
        metadata
            .validate(AssistantSourceKind::NativeStatus, &"b".repeat(64), 300)
            .is_err()
    );
    assert!(
        metadata
            .validate(AssistantSourceKind::Receipt, &"b".repeat(64), 200)
            .is_err()
    );
}

#[test]
#[allow(clippy::field_reassign_with_default)]
fn capture_binds_native_projection_bytes_and_keeps_the_original_source_policy_revision() {
    let content = "Verified sent metadata with omitted attachment labels";
    let message = crate::model_message_labels::model_bound_user_message(
        "native".into(),
        content.into(),
        DestinationIdentity::Model {
            connection_id: "gateway".into(),
            connection_revision: 1,
            model_id: "model".into(),
            profile_revision: 1,
        },
    )
    .unwrap();
    let mut native = metadata();
    native.projected_content_sha256 = format!("{:x}", Sha256::digest(content.as_bytes()));
    native.projection_partial = true;
    let policy = owner_text_policy(1, false);
    let mut platform = AssistantDataPolicy::default();
    platform.revision = 2;
    let capture = HistoryCapture {
        native: Some(&native),
        actor_id: "owner",
        device_ref: "device",
        directory: None,
        conversation_id: "conversation",
        root_conversation_id: "conversation",
        native_id: "native-receipt",
        content,
        envelope: message.data_envelope.as_ref().unwrap(),
        source_policy: &policy,
        dependencies: &[],
        accepted_input_seq: 1,
        kind: AssistantSourceKind::Receipt,
        role: "tool",
        recorded_at_unix_ms: 200,
        statement_timezone: "UTC",
        revision: 1,
    };
    let prepared = prepare_capture(&capture, &platform).unwrap().unwrap();
    assert!(prepared.index.partial);
    assert_eq!(prepared.proof.source_policy.policy_revision, 1);
    let wrong = HistoryCapture {
        content: "A model says this was sent",
        ..capture
    };
    assert!(prepare_capture(&wrong, &platform).is_err());
}

#[test]
fn inventory_preserves_unknown_revisions_and_prioritizes_unresolved_native_tasks() {
    let items = (0..6)
        .map(|index| NativeHistoryStatus {
            native_id: format!("native-task-{index}"),
            kind: NativeHistoryTaskKind::Work,
            state: if index == 4 {
                "outcome_unknown"
            } else if index == 2 {
                "dispatched"
            } else {
                "done"
            }
            .into(),
            revision: None,
            observed_at_unix_ms: 100 + index,
        })
        .collect();
    let inventory = NativeHistorySnapshot::bounded(
        items,
        true,
        200,
        desk_agent_protocol::ai_assistant::limits::AssistantProcessingLimits::default()
            .native_status_items as usize,
    )
    .unwrap();
    let summary = inventory
        .summary(
            "native-source",
            1,
            desk_agent_protocol::ai_assistant::limits::AssistantProcessingLimits::default()
                .native_status_model_items as usize,
        )
        .unwrap();
    assert_eq!(summary.items.len(), 3);
    assert_eq!(summary.additional_count, 3);
    assert_eq!(summary.pending_count, 2);
    assert_eq!(summary.unknown_count, 1);
    assert_eq!(summary.items[0].state, "outcome_unknown");
    assert_eq!(summary.items[1].state, "dispatched");
    assert!(
        summary
            .items
            .iter()
            .all(|item| item.revision.is_none() && item.historical)
    );
    assert!(!summary.no_native_task);
    assert!(summary.has_more);
    assert_eq!(summary.last_recorded_at_unix_ms, 200);
}

#[test]
fn only_a_complete_empty_native_inventory_proves_no_native_task_and_bounds_are_explicit() {
    for complete in [false, true] {
        let inventory = NativeHistorySnapshot::bounded(
            Vec::new(),
            complete,
            200,
            desk_agent_protocol::ai_assistant::limits::AssistantProcessingLimits::default()
                .native_status_items as usize,
        )
        .unwrap();
        let summary = inventory
            .summary(
                "native-source",
                1,
                desk_agent_protocol::ai_assistant::limits::AssistantProcessingLimits::default()
                    .native_status_model_items as usize,
            )
            .unwrap();
        assert_eq!(summary.no_native_task, complete);
        assert_eq!(summary.has_more, !complete);
    }
    let items = (0..40)
        .map(|index| NativeHistoryStatus {
            native_id: format!("task-{index}"),
            kind: NativeHistoryTaskKind::Subagent,
            state: "running".into(),
            revision: Some(1),
            observed_at_unix_ms: 200,
        })
        .collect();
    let inventory = NativeHistorySnapshot::bounded(
        items,
        true,
        200,
        desk_agent_protocol::ai_assistant::limits::AssistantProcessingLimits::default()
            .native_status_items as usize,
    )
    .unwrap();
    assert!(inventory.items.len() <= 24);
    assert!(!inventory.complete);
    assert_eq!(inventory.items.len(), 24);
    let mut invalid = inventory.clone();
    invalid.items[0].revision = Some(0);
    assert!(invalid.validate(200).is_err());
    let mut invalid = inventory.clone();
    invalid.items[0].observed_at_unix_ms = 201;
    assert!(invalid.validate(200).is_err());
    let mut invalid = inventory.clone();
    invalid.items[1] = invalid.items[0].clone();
    assert!(invalid.validate(200).is_err());
}

#[test]
fn original_native_inventory_has_stable_content_only_history_even_before_owner_chat() {
    let inventory = NativeHistorySnapshot::bounded(
        Vec::new(),
        true,
        200,
        desk_agent_protocol::ai_assistant::limits::AssistantProcessingLimits::default()
            .native_status_items as usize,
    )
    .unwrap();
    let make = |hash: &str| {
        NativeStatusHistoryProjection::new(
            "owner",
            "device",
            "schedule-run-native",
            hash,
            inventory.clone(),
            1,
        )
        .unwrap()
    };
    let original = make(&"a".repeat(64));
    assert_eq!(
        original.capture(0, None).native_id,
        make(&"a".repeat(64)).capture(0, None).native_id
    );
    let capture = original.capture(0, None);
    assert_eq!(capture.kind, AssistantSourceKind::NativeStatus);
    assert_eq!(capture.statement_timezone, "UTC");
    assert_eq!(capture.recorded_at_unix_ms, 200);
    assert!(!capture.source_policy.independent_fact_allowed);
    assert!(capture.envelope.allowed_destinations.is_empty());
    assert!(capture.native.unwrap().time.is_none());
    assert!(
        prepare_capture(&capture, &AssistantDataPolicy::default())
            .unwrap()
            .is_some()
    );
    let mut wrong = original.capture(0, None);
    wrong.role = "user";
    assert!(prepare_capture(&wrong, &AssistantDataPolicy::default()).is_err());
    let mut wrong = original.capture(0, None);
    wrong.native = None;
    assert!(prepare_capture(&wrong, &AssistantDataPolicy::default()).is_err());
    assert!(!crate::ai_assistant::source::zero_owner_input_allowed(
        AssistantSourceKind::Receipt,
        "tool",
        "schedule-run-native",
        "schedule-run-native",
        capture.native
    ));
}

#[test]
fn native_inventory_uses_its_item_contract_without_a_separate_metadata_byte_ceiling() {
    let items = (0..24)
        .map(|index| NativeHistoryStatus {
            native_id: format!("task-{index}-{}", "x".repeat(240)),
            kind: NativeHistoryTaskKind::Subagent,
            state: "running".repeat(9),
            revision: Some(1),
            observed_at_unix_ms: 200,
        })
        .collect();
    let inventory = NativeHistorySnapshot::bounded(
        items,
        true,
        200,
        desk_agent_protocol::ai_assistant::limits::AssistantProcessingLimits::default()
            .native_status_items as usize,
    )
    .unwrap();
    assert!(serde_json::to_vec(&inventory).unwrap().len() > 8192);
    assert_eq!(inventory.items.len(), 24);
    assert!(inventory.complete);
    inventory.validate(200).unwrap();
}

#[test]
fn native_inventory_and_model_summary_use_configured_counts_without_a_decode_cap() {
    let items = (0..60)
        .map(|index| NativeHistoryStatus {
            native_id: format!("native-{index}"),
            kind: NativeHistoryTaskKind::Work,
            state: "running".into(),
            revision: Some(1),
            observed_at_unix_ms: 100,
        })
        .collect::<Vec<_>>();
    let inventory = NativeHistorySnapshot::bounded(items, true, 100, 40).unwrap();
    assert_eq!(inventory.items.len(), 40);
    assert!(!inventory.complete);
    inventory.validate(100).unwrap();
    let summary = inventory.summary("native-source", 1, 8).unwrap();
    assert_eq!(summary.items.len(), 8);
    assert_eq!(summary.additional_count, 32);
    assert!(summary.has_more);
}
