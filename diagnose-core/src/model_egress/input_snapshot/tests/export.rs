use super::*;
use crate::{
    chat::{ChatMessage, ModelTurn, StopReason, ToolCall},
    model_egress::{
        AuthorizedModelRequest, ModelEgressPolicy, NativeModelOutputEvidence,
        NativeModelOutputSources,
    },
    model_message_labels::{ReadResultLabel, read_result_envelope},
    prompt::ResponseFormatSpec,
    seam::{ModelRequest, ToolOutputFormat, ToolRunOutput},
};

fn authorized(with_background: bool) -> (ModelEgressPolicy, AuthorizedModelRequest, DataEnvelope) {
    let tool = crate::web_research::WEB_FETCH_TOOL_NAME;
    let output = ToolRunOutput {
        format: ToolOutputFormat::Text,
        content: "bounded public result".into(),
        image_data_url: None,
        document_preview: None,
    };
    let mut original = read_result_envelope(
        crate::ai_assistant::ai_assistant_provider_registry_ref(),
        &ToolCall {
            id: "read".into(),
            name: tool.into(),
            arguments_json: "{}".into(),
        },
        &output,
        ReadResultLabel {
            envelope_id: "original-result".into(),
            observation_id: "original-observation".into(),
            source_object_id: None,
            observed_at_unix_ms: 100,
        },
    )
    .unwrap();
    original.retention.expires_at_unix_ms = Some(5000);
    if with_background {
        original.provenance.source_envelope_ids = vec!["original-background".into()];
    }
    let mut message = ChatMessage::tool_result("read-message", "read", output.content);
    message.data_envelope = Some(original.clone());
    let policy = ModelEgressPolicy {
        destination: DestinationIdentity::Model {
            connection_id: "gateway".into(),
            connection_revision: 1,
            model_id: "model".into(),
            profile_revision: 1,
        },
        selected_source_tools: [tool.into()].into_iter().collect(),
        export_authorization_id: "actual-export".into(),
        now_unix_ms: 100,
        byte_cap: crate::sink_authorizer::MAX_SINK_BYTES,
        permission_resume: false,
    };
    let request = policy
        .authorize_request(ModelRequest::text_only(
            vec![message],
            ResponseFormatSpec::None,
        ))
        .unwrap();
    (policy, request, original)
}

#[test]
fn actual_authorizer_keeps_the_request_and_preserves_original_background_beneath_its_export_alias()
{
    let (_, authorized, original) = authorized(true);
    assert_eq!(
        authorized.request.messages[0].data_envelope.as_ref(),
        Some(&original)
    );
    assert_eq!(
        authorized.input_envelopes[0].provenance.source_envelope_ids,
        std::slice::from_ref(&original.envelope_id)
    );
    assert_ne!(
        authorized.input_envelopes[0].envelope_id,
        original.envelope_id
    );
    let snapshot = authorized.input_snapshot().unwrap();
    assert_eq!(snapshot.exports.len(), 1);
    assert_eq!(snapshot.exports[0].source, original);
    assert_eq!(snapshot.policy_sources(), vec![&original]);
    assert_eq!(
        snapshot.exports[0].source.provenance.source_envelope_ids,
        ["original-background"]
    );
    snapshot
        .validate_export_authorization("actual-export")
        .unwrap();
    assert!(
        snapshot
            .validate_export_authorization("different-export")
            .is_err()
    );
    assert_eq!(
        serde_json::from_str::<NativeModelInputSnapshot>(
            &serde_json::to_string(&snapshot).unwrap()
        )
        .unwrap(),
        snapshot
    );
}

#[test]
fn native_export_metadata_rejects_identity_content_constraint_and_recipient_replacement() {
    let (_, authorized, _) = authorized(true);
    let snapshot = authorized.input_snapshot().unwrap();
    for case in 0..7 {
        let mut changed = snapshot.clone();
        match case {
            0 => changed.exports[0].source_message_id = "other-message".into(),
            1 => changed.exports[0].source.retention.expires_at_unix_ms = Some(6000),
            2 => changed.exports[0].source.sensitivity = Sensitivity::Secret,
            3 => {
                changed.exports[0].source.provenance.source_object_id = Some("other-object".into())
            }
            4 => changed.exports[0].transformation.input_envelope_ids.clear(),
            5 => {
                changed.exports[0].transformation.output_envelope_id = "not-an-audited-input".into()
            }
            _ => {
                changed.destination = DestinationIdentity::Model {
                    connection_id: "other".into(),
                    connection_revision: 1,
                    model_id: "model".into(),
                    profile_revision: 1,
                }
            }
        }
        assert!(changed.fingerprint().is_err(), "case {case}");
    }
}

#[test]
fn original_background_changes_are_bound_even_when_the_projected_body_and_thin_lineage_do_not_change()
 {
    let (policy, authorized, _) = authorized(true);
    let snapshot = authorized.input_snapshot().unwrap();
    let inputs =
        project_model_input_lineage(&authorized.audit, &authorized.input_envelopes).unwrap();
    let output = policy
        .derive_model_output_envelope(
            &ModelTurn {
                text: "Research conclusion".into(),
                stop_reason: StopReason::EndTurn,
                ..Default::default()
            },
            &authorized.input_envelopes,
        )
        .unwrap();
    let evidence = NativeModelOutputEvidence::from_receipt(
        "physical",
        "root",
        "actual-export",
        100,
        200,
        &authorized.audit,
        &inputs,
        &snapshot,
        &output,
    )
    .unwrap();
    let mut changed = snapshot;
    changed.exports[0].source.provenance.source_envelope_ids = vec!["different-background".into()];
    assert_ne!(
        changed.fingerprint().unwrap(),
        evidence.input_snapshot_sha256
    );
    let sources = NativeModelOutputSources {
        evidence,
        inputs,
        input_snapshot: changed,
    };
    assert!(sources.validate(&output).is_err());
}

#[test]
fn policy_registration_uses_the_exact_original_provider_label_and_preserves_its_hard_deadline() {
    let (_, authorized, original) = authorized(false);
    let snapshot = authorized.input_snapshot().unwrap();
    let registration =
        crate::ai_assistant::source_registry::NativeInputPolicyRegistration::capture(
            &snapshot,
            crate::ai_assistant::source_registry::SOURCE_CONTRACT_REVISION,
            1,
        )
        .unwrap();
    let policy = registration
        .complete_leaf_text_policy(&snapshot)
        .unwrap()
        .unwrap();
    assert!(policy.history_retention_allowed);
    assert!(!policy.independent_fact_allowed);
    assert_eq!(
        policy.content_hard_expires_at_unix_ms,
        original.retention.expires_at_unix_ms
    );
    assert_eq!(
        registration.leaves[0].as_ref().unwrap().contract,
        crate::ai_assistant::source_registry::RegisteredSourceContract::PublicResearchResult
    );
}

#[test]
fn controls_keep_the_original_policy_across_an_actual_native_export_alias() {
    let (_, authorized, original) = authorized(false);
    let control = crate::model_message_labels::internal_tool_result_envelope(
        Some(&authorized.input_envelopes[0]),
        "control",
        "observed",
        "runtime_context",
    )
    .unwrap()
    .unwrap();
    let mut audit = authorized.audit.clone();
    audit.envelope_ids.push(control.envelope_id.clone());
    audit.total_bytes += 8;
    let original_snapshot = authorized.input_snapshot().unwrap();
    let snapshot = NativeModelInputSnapshot::capture_with_exports(
        &audit,
        &[authorized.input_envelopes[0].clone(), control],
        &original_snapshot.exports,
    )
    .unwrap();
    let registration =
        crate::ai_assistant::source_registry::NativeInputPolicyRegistration::capture(
            &snapshot,
            crate::ai_assistant::source_registry::SOURCE_CONTRACT_REVISION,
            1,
        )
        .unwrap();
    let policy = registration
        .complete_leaf_text_policy(&snapshot)
        .unwrap()
        .unwrap();
    assert!(policy.history_retention_allowed && !policy.independent_fact_allowed);
    assert_eq!(
        policy.content_hard_expires_at_unix_ms,
        original.retention.expires_at_unix_ms
    );
    registration.validate_registered_leaves(&snapshot).unwrap();
    assert_eq!(
        registration.leaves[1].as_ref().unwrap().contract,
        crate::ai_assistant::source_registry::RegisteredSourceContract::NativeControl
    );
}
