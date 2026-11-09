use super::*;
mod derived;
use crate::{
    ai_assistant::*,
    chat::{ChatMessage, ChatRole, ToolCall},
    model_egress::ModelEgressPolicy,
    model_message_labels::{ReadResultLabel, model_bound_user_message, read_result_envelope},
    prompt::ResponseFormatSpec,
    seam::{ModelRequest, ToolOutputFormat, ToolRunOutput},
};
use desk_agent_protocol::{
    ai_assistant::user_instructions::UserInstructions, data_lineage::DestinationIdentity,
};

fn snapshot(inputs: Vec<DataEnvelope>) -> crate::model_egress::NativeModelInputSnapshot {
    let audit = crate::sink_authorizer::SinkProjectionAudit {
        destination: destination(),
        envelope_ids: inputs
            .iter()
            .map(|input| input.envelope_id.clone())
            .collect(),
        total_bytes: 32,
    };
    crate::model_egress::NativeModelInputSnapshot::capture(&audit, &inputs).unwrap()
}

fn destination() -> DestinationIdentity {
    DestinationIdentity::Model {
        connection_id: "gateway".into(),
        connection_revision: 1,
        model_id: "model".into(),
        profile_revision: 1,
    }
}

fn user_with_id(id: &str) -> DataEnvelope {
    model_bound_user_message(id.into(), "Another owner input".into(), destination())
        .unwrap()
        .data_envelope
        .unwrap()
}

fn user() -> DataEnvelope {
    model_bound_user_message(
        "owner-input".into(),
        "I prefer concise Chinese replies".into(),
        destination(),
    )
    .unwrap()
    .data_envelope
    .unwrap()
}

fn provider_result(capability: &str) -> DataEnvelope {
    let registry = ai_assistant_provider_registry_ref();
    let call = ToolCall {
        id: "native-read".into(),
        name: registry
            .capability(capability)
            .unwrap()
            .wire
            .tool_name
            .clone(),
        arguments_json: "{}".into(),
    };
    read_result_envelope(
        registry,
        &call,
        &ToolRunOutput {
            format: ToolOutputFormat::Text,
            content: "bounded native result".into(),
            image_data_url: None,
            document_preview: None,
        },
        ReadResultLabel {
            envelope_id: "native-result".into(),
            observation_id: "observation-result".into(),
            source_object_id: None,
            observed_at_unix_ms: 100,
        },
    )
    .unwrap()
}

#[test]
fn native_read_snapshots_keep_actual_parent_policies_without_reclassifying_old_results() {
    let owner = user();
    let mut read = provider_result(SYSTEM_INFO_CAPABILITY_ID);
    read.allowed_destinations = vec![destination()];
    read.provenance.source_envelope_ids = vec![owner.envelope_id.clone()];
    let policy = |inputs| {
        let input = snapshot(inputs);
        let registration =
            NativeInputPolicyRegistration::capture(&input, SOURCE_CONTRACT_REVISION, 1).unwrap();
        registration.complete_leaf_text_policy(&input).unwrap()
    };
    let retained = policy(vec![owner.clone(), read.clone()]).unwrap();
    assert!(retained.history_retention_allowed && !retained.no_copy);
    assert!(!retained.independent_fact_allowed);
    assert!(policy(vec![read.clone()]).is_none());

    let mut restricted = owner.clone();
    restricted.retention.expires_at_unix_ms = Some(500);
    let retained = policy(vec![restricted.clone(), read.clone()]).unwrap();
    assert_eq!(retained.content_hard_expires_at_unix_ms, Some(500));
    restricted.sensitivity = Sensitivity::Secret;
    let retained = policy(vec![restricted, read.clone()]).unwrap();
    assert!(!retained.history_retention_allowed && retained.no_copy);

    let mut old = read.clone();
    if let ContentRef::ImmutableBlob { blob_id, .. } = &mut old.content {
        *blob_id = "ordinary-old-observation".into();
    }
    assert!(policy(vec![owner.clone(), old]).is_none());
    let mut forged = read.clone();
    forged.provenance.source_provider_id = "unknown-reader".into();
    assert!(policy(vec![owner, forged]).is_none());
    let mut other = read.clone();
    other.envelope_id = "other-native-result".into();
    other.provenance.source_envelope_ids = vec![read.envelope_id.clone()];
    read.provenance.source_envelope_ids = vec![other.envelope_id.clone()];
    assert!(policy(vec![read, other]).is_none());
}

#[test]
fn owner_and_personalization_contracts_keep_their_distinct_purposes() {
    let owner = registered_leaf(&user(), 1, SOURCE_CONTRACT_REVISION)
        .unwrap()
        .unwrap();
    assert_eq!(owner.contract, RegisteredSourceContract::OwnerText);
    assert!(
        owner.policy.history_retention_allowed
            && owner.policy.independent_fact_allowed
            && owner.policy.derived_visible_text_allowed
    );
    owner
        .validate_leaf(&user(), SOURCE_CONTRACT_REVISION)
        .unwrap();
    let frozen = settings::freeze_instructions(
        &UserInstructions {
            revision: 1,
            content: "Prefer concise answers".into(),
        },
        1,
        100,
    )
    .unwrap();
    let envelope = settings::instructions_projection(&frozen, "root", destination())
        .unwrap()
        .unwrap()
        .data_envelope
        .unwrap();
    let preferences = registered_leaf(&envelope, 1, SOURCE_CONTRACT_REVISION)
        .unwrap()
        .unwrap();
    assert_eq!(
        preferences.contract,
        RegisteredSourceContract::FrozenPersonalization
    );
    assert!(
        !preferences.policy.history_retention_allowed
            && !preferences.policy.independent_fact_allowed
    );
    assert!(preferences.policy.derived_visible_text_allowed);
}

#[test]
fn public_system_registration_uses_the_actual_native_projector_and_never_public_sensitivity_alone()
{
    let request = ModelRequest::text_only(
        vec![ChatMessage::text(
            crate::agentic_prompt::AGENTIC_SYSTEM_MESSAGE_ID,
            ChatRole::System,
            "Public assistant runtime instructions",
        )],
        ResponseFormatSpec::None,
    );
    let projected = ModelEgressPolicy {
        destination: destination(),
        selected_source_tools: Default::default(),
        export_authorization_id: "export".into(),
        now_unix_ms: 100,
        byte_cap: crate::sink_authorizer::MAX_SINK_BYTES,
        permission_resume: false,
    }
    .authorize_request(request)
    .unwrap();
    let envelope = projected.input_envelopes.first().unwrap();
    assert_eq!(
        registered_leaf(envelope, 1, SOURCE_CONTRACT_REVISION)
            .unwrap()
            .unwrap()
            .contract,
        RegisteredSourceContract::PublicSystem
    );
    let mut forged = envelope.clone();
    forged.provenance.source_tool_name = "model-claimed-public-prompt".into();
    let unknown = registered_leaf(&forged, 1, SOURCE_CONTRACT_REVISION)
        .unwrap()
        .unwrap();
    assert_eq!(unknown.contract, RegisteredSourceContract::Unknown);
    assert!(
        unknown.policy.no_copy
            && unknown.policy.no_export
            && !unknown.policy.derived_visible_text_allowed
    );
}

#[test]
fn compiled_provider_capabilities_allow_derived_text_without_archiving_full_results_or_promoting_them_to_facts()
 {
    for (capability, expected) in [
        (
            WEB_RESEARCH_FETCH_CAPABILITY_ID,
            RegisteredSourceContract::PublicResearchResult,
        ),
        (
            WEB_RESEARCH_SEARCH_CAPABILITY_ID,
            RegisteredSourceContract::PublicResearchResult,
        ),
        (
            SYSTEM_INFO_CAPABILITY_ID,
            RegisteredSourceContract::DeviceObservation,
        ),
        (
            DESKTOP_SESSION_CAPABILITY_ID,
            RegisteredSourceContract::DeviceObservation,
        ),
        (
            DESKTOP_UI_CAPABILITY_ID,
            RegisteredSourceContract::UiObservation,
        ),
        (
            BROWSER_SNAPSHOT_CAPABILITY_ID,
            RegisteredSourceContract::UiObservation,
        ),
        (
            CURRENT_SCREEN_CAPABILITY_ID,
            RegisteredSourceContract::MediaObservation,
        ),
    ] {
        let envelope = provider_result(capability);
        let registered = registered_leaf(&envelope, 1, SOURCE_CONTRACT_REVISION)
            .unwrap()
            .unwrap();
        assert_eq!(registered.contract, expected, "{capability}");
        assert!(
            !registered.policy.history_retention_allowed
                && !registered.policy.independent_fact_allowed
        );
        assert!(registered.policy.derived_visible_text_allowed);
        let derived =
            source::derive_visible_text_policy(std::slice::from_ref(&registered.policy), 1)
                .unwrap();
        assert!(derived.history_retention_allowed && derived.derived_visible_text_allowed);
        assert!(!derived.independent_fact_allowed);
        registered
            .validate_leaf(&envelope, SOURCE_CONTRACT_REVISION)
            .unwrap();
    }
}

#[test]
fn secret_unknown_or_unregistered_private_results_cannot_get_new_retention_by_renaming_or_public_labels()
 {
    let mut secret = user();
    secret.sensitivity = Sensitivity::Secret;
    let mut provider_mismatch = provider_result(WEB_RESEARCH_FETCH_CAPABILITY_ID);
    provider_mismatch.provenance.source_provider_id = "unregistered-provider".into();
    let log = provider_result(SYSTEM_LOG_CAPABILITY_ID);
    let mut unknown = user();
    unknown.provenance.source_provider_id = "model-prose".into();
    unknown.sensitivity = Sensitivity::Public;
    for envelope in [secret, provider_mismatch, log, unknown] {
        let registered = registered_leaf(&envelope, 1, SOURCE_CONTRACT_REVISION)
            .unwrap()
            .unwrap();
        assert!(
            registered.policy.no_copy
                && registered.policy.no_export
                && !registered.policy.derived_visible_text_allowed
        );
        let derived = source::derive_visible_text_policy(&[registered.policy], 1).unwrap();
        assert!(!derived.history_retention_allowed && !derived.independent_fact_allowed);
    }
}

#[test]
fn registration_binds_full_labels_original_revision_and_retention_without_guessing_object_freshness()
 {
    let original = user();
    let registered = registered_leaf(&original, 1, SOURCE_CONTRACT_REVISION)
        .unwrap()
        .unwrap();
    for case in 0..4 {
        let mut changed = original.clone();
        match case {
            0 => changed.retention.expires_at_unix_ms = Some(1000),
            1 => changed.provenance.source_object_id = Some("other-input".into()),
            2 => changed.allowed_destinations.clear(),
            _ => changed.sensitivity = Sensitivity::Sensitive,
        }
        assert!(
            registered
                .validate_leaf(&changed, SOURCE_CONTRACT_REVISION)
                .is_err()
        );
    }
    assert!(registered_leaf(&original, 1, SOURCE_CONTRACT_REVISION + 1).is_err());
    let mut derived = original.clone();
    derived
        .provenance
        .source_envelope_ids
        .push("original-parent".into());
    assert!(
        registered_leaf(&derived, 1, SOURCE_CONTRACT_REVISION)
            .unwrap()
            .is_none()
    );
    let mut live = provider_result(CURRENT_SCREEN_CAPABILITY_ID);
    live.provenance.source_object_id = Some(format!("live-target:sha256:{}", "a".repeat(64)));
    assert!(
        registered_leaf(&live, 1, SOURCE_CONTRACT_REVISION)
            .unwrap()
            .is_none()
    );
    let mut bounded = provider_result(WEB_RESEARCH_FETCH_CAPABILITY_ID);
    bounded.retention.expires_at_unix_ms = Some(1000);
    let policy = registered_leaf(&bounded, 1, SOURCE_CONTRACT_REVISION)
        .unwrap()
        .unwrap()
        .policy;
    assert_eq!(policy.content_hard_expires_at_unix_ms, Some(1000));
    assert!(policy.reference_fresh_until_unix_ms.is_none());
    let derived = source::derive_visible_text_policy(&[policy], 1).unwrap();
    assert_eq!(derived.content_hard_expires_at_unix_ms, Some(1000));
}

#[test]
fn native_snapshot_registration_keeps_every_actual_input_with_a_bounded_shared_policy_dictionary() {
    let original = snapshot(vec![user(); 300]);
    let registration =
        NativeInputPolicyRegistration::capture(&original, SOURCE_CONTRACT_REVISION, 1).unwrap();
    registration.validate_registered_leaves(&original).unwrap();
    assert_eq!(registration.leaves.len(), 300);
    assert_eq!(registration.policies.len(), 1);
    assert!(
        serde_json::to_vec(&registration).unwrap().len()
            < serde_json::to_vec(&original).unwrap().len()
    );
    assert!(
        registration
            .complete_leaf_text_policy(&original)
            .unwrap()
            .unwrap()
            .history_retention_allowed
    );
    let mut trimmed = registration.clone();
    trimmed.leaves.pop();
    assert!(
        trimmed
            .validate_metadata(&original, SOURCE_CONTRACT_REVISION, 1)
            .is_err()
    );
    let mut dropped = registration;
    dropped.leaves.fill(None);
    dropped.policies.clear();
    dropped
        .validate_metadata(&original, SOURCE_CONTRACT_REVISION, 1)
        .unwrap();
    assert!(dropped.validate_registered_leaves(&original).is_err());
}

#[test]
fn unresolved_native_parent_prevents_complete_text_policy_and_unknown_original_contracts_cannot_be_upgraded()
 {
    let mut with_parent = user();
    with_parent.envelope_id = "derived-input".into();
    with_parent
        .provenance
        .source_envelope_ids
        .push("original-parent".into());
    let original = snapshot(vec![user(), with_parent]);
    let registration =
        NativeInputPolicyRegistration::capture(&original, SOURCE_CONTRACT_REVISION, 1).unwrap();
    assert!(registration.leaves[0].is_some() && registration.leaves[1].is_none());
    assert!(
        registration
            .complete_leaf_text_policy(&original)
            .unwrap()
            .is_none()
    );
    let unavailable =
        NativeInputPolicyRegistration::capture(&original, SOURCE_CONTRACT_REVISION + 1, 1).unwrap();
    unavailable
        .validate_metadata(&original, SOURCE_CONTRACT_REVISION + 1, 1)
        .unwrap();
    assert!(unavailable.leaves.iter().all(Option::is_none) && unavailable.policies.is_empty());
    assert!(unavailable.complete_leaf_text_policy(&original).is_err());
}

#[test]
fn policy_dictionary_has_one_encoding_in_first_use_order() {
    let original = snapshot(vec![user(), user_with_id("second")]);
    let registration =
        NativeInputPolicyRegistration::capture(&original, SOURCE_CONTRACT_REVISION, 1).unwrap();
    // Equal policies share one entry.
    let indices: Vec<_> = registration
        .leaves
        .iter()
        .flatten()
        .map(|leaf| leaf.policy_index)
        .collect();
    assert_eq!(indices, vec![0, 0]);
    assert_eq!(registration.policies.len(), 1);
    registration
        .validate_metadata(&original, SOURCE_CONTRACT_REVISION, 1)
        .unwrap();
    let mut duplicate = registration.clone();
    duplicate.policies.push(duplicate.policies[0].clone());
    assert!(
        duplicate
            .validate_metadata(&original, SOURCE_CONTRACT_REVISION, 1)
            .is_err()
    );
    let mut out_of_range = registration.clone();
    out_of_range.leaves[0].as_mut().unwrap().policy_index = 99;
    assert!(
        out_of_range
            .validate_metadata(&original, SOURCE_CONTRACT_REVISION, 1)
            .is_err()
    );
}

#[test]
fn native_snapshot_cannot_replace_a_restrictive_policy_or_rebind_a_full_input_label() {
    let mut secret = user();
    secret.sensitivity = Sensitivity::Secret;
    let original = snapshot(vec![secret]);
    let registration =
        NativeInputPolicyRegistration::capture(&original, SOURCE_CONTRACT_REVISION, 1).unwrap();
    assert!(
        !registration
            .complete_leaf_text_policy(&original)
            .unwrap()
            .unwrap()
            .history_retention_allowed
    );
    let mut forged = registration.clone();
    let permissive = source::owner_text_policy(1, false);
    forged.leaves[0].as_mut().unwrap().policy_index = 0;
    forged.leaves[0].as_mut().unwrap().contract = RegisteredSourceContract::OwnerText;
    forged.policies = vec![permissive];
    forged
        .validate_metadata(&original, SOURCE_CONTRACT_REVISION, 1)
        .unwrap();
    assert!(forged.validate_registered_leaves(&original).is_err());
}

#[test]
fn native_controls_require_their_actual_parents_and_never_become_independent_facts() {
    for tool in [
        "owner_context_time",
        "context_review",
        "runtime_context",
        "input_watermark",
        "latest_input_projection",
        "schedule_activation",
        "scheduled_task_input",
        "invalid_tool_arguments",
        "memory_batch_rejected",
        "manage_memories",
        "memory_control_rejected",
        "schedule_proposal",
        "schedule_query",
        "schedule_cancellation",
        "schedule_proposal_failed",
        crate::issue_reply::PREPARE_TOOL_NAME,
        crate::skills::LOAD_TOOL,
    ] {
        let owner = user();
        let control = crate::model_message_labels::internal_tool_result_envelope(
            Some(&owner),
            "control",
            "native observation",
            tool,
        )
        .unwrap()
        .unwrap();
        assert!(
            registered_leaf(&control, 1, SOURCE_CONTRACT_REVISION)
                .unwrap()
                .is_none()
        );
        let original = snapshot(vec![control.clone(), owner]);
        let registration =
            NativeInputPolicyRegistration::capture(&original, SOURCE_CONTRACT_REVISION, 1).unwrap();
        registration.validate_registered_leaves(&original).unwrap();
        assert_eq!(
            registration.leaves[0].as_ref().unwrap().contract,
            RegisteredSourceContract::NativeControl
        );
        let policy = registration
            .complete_leaf_text_policy(&original)
            .unwrap()
            .unwrap();
        assert!(policy.history_retention_allowed && !policy.independent_fact_allowed);
        let mut historical = registration.clone();
        historical.leaves[0] = None;
        // Re-encode the remaining leaves in first-use order.
        let original_policies = std::mem::take(&mut historical.policies);
        for leaf in historical.leaves.iter_mut().flatten() {
            let policy = &original_policies[leaf.policy_index as usize];
            leaf.policy_index = match historical.policies.iter().position(|p| p == policy) {
                Some(index) => index as u32,
                None => {
                    historical.policies.push(policy.clone());
                    (historical.policies.len() - 1) as u32
                }
            };
        }
        historical.validate_registered_leaves(&original).unwrap();
        assert!(
            historical
                .complete_leaf_text_policy(&original)
                .unwrap()
                .is_none()
        );
        let missing = snapshot(vec![control]);
        let missing =
            NativeInputPolicyRegistration::capture(&missing, SOURCE_CONTRACT_REVISION, 1).unwrap();
        assert!(missing.leaves[0].is_none(), "{tool}");
    }
}

#[test]
fn native_control_chains_and_real_resume_and_delegation_producers_keep_parent_constraints() {
    let mut owner = user();
    owner.retention.expires_at_unix_ms = Some(1000);
    let mut first = crate::model_message_labels::internal_tool_result_envelope(
        Some(&owner),
        "first",
        "accepted",
        "owner_context_time",
    )
    .unwrap()
    .unwrap();
    first.retention.expires_at_unix_ms = Some(500);
    let second = crate::model_message_labels::internal_tool_result_envelope(
        Some(&first),
        "second",
        "ready",
        "context_review",
    )
    .unwrap()
    .unwrap();
    let original = snapshot(vec![second, first, owner.clone()]);
    let registration =
        NativeInputPolicyRegistration::capture(&original, SOURCE_CONTRACT_REVISION, 1).unwrap();
    let policy = registration
        .complete_leaf_text_policy(&original)
        .unwrap()
        .unwrap();
    assert!(policy.history_retention_allowed && !policy.independent_fact_allowed);
    assert_eq!(policy.content_hard_expires_at_unix_ms, Some(500));
    let owner_message =
        model_bound_user_message("owner-input".into(), "Original task".into(), destination())
            .unwrap();
    let policy = ModelEgressPolicy {
        destination: destination(),
        selected_source_tools: Default::default(),
        export_authorization_id: "native-resume".into(),
        now_unix_ms: 100,
        byte_cap: crate::sink_authorizer::MAX_SINK_BYTES,
        permission_resume: false,
    };
    let mut messages = vec![
        owner_message.clone(),
        crate::permission_resume::authorized_scheduled_resume_message(
            "scheduled".into(),
            &policy,
            &owner_message,
        )
        .unwrap(),
        crate::permission_resume::authorized_permission_resume_message(
            "permission".into(),
            &policy,
            &owner_message,
        )
        .unwrap(),
        crate::subagent::projection::runtime_message(
            "delegation",
            &serde_json::json!({"tasks":[]}),
            &[owner_message.data_envelope.clone().unwrap()],
        )
        .unwrap(),
    ];
    let original = snapshot(
        messages
            .drain(..)
            .map(|message| message.data_envelope.unwrap())
            .collect(),
    );
    let registration =
        NativeInputPolicyRegistration::capture(&original, SOURCE_CONTRACT_REVISION, 1).unwrap();
    assert!(
        registration.leaves[1..]
            .iter()
            .all(|leaf| leaf.as_ref().unwrap().contract == RegisteredSourceContract::NativeControl)
    );
    assert!(
        registration
            .complete_leaf_text_policy(&original)
            .unwrap()
            .unwrap()
            .history_retention_allowed
    );
}

#[test]
fn a_native_control_cannot_launder_unknown_private_or_secret_parent_content() {
    let mut secret = user();
    secret.sensitivity = Sensitivity::Secret;
    let mut unknown = user();
    unknown.provenance.source_provider_id = "unknown-provider".into();
    // Model disclosure of a private result does not authorize retaining it.
    let mut private = provider_result(SYSTEM_LOG_CAPABILITY_ID);
    private.allowed_destinations.push(destination());
    for parent in [secret, unknown, private] {
        for tool in [
            "runtime_context",
            "invalid_tool_arguments",
            "memory_batch_rejected",
            "manage_memories",
            "memory_control_rejected",
            "schedule_proposal",
            "schedule_query",
            "schedule_cancellation",
            "schedule_proposal_failed",
            crate::issue_reply::PREPARE_TOOL_NAME,
            crate::skills::LOAD_TOOL,
        ] {
            let control = crate::model_message_labels::internal_tool_result_envelope(
                Some(&parent),
                "control",
                "native result",
                tool,
            )
            .unwrap()
            .unwrap();
            let original = snapshot(vec![control, parent.clone()]);
            let registration =
                NativeInputPolicyRegistration::capture(&original, SOURCE_CONTRACT_REVISION, 1)
                    .unwrap();
            let policy = registration
                .complete_leaf_text_policy(&original)
                .unwrap()
                .unwrap();
            assert!(
                !policy.history_retention_allowed
                    && !policy.independent_fact_allowed
                    && policy.no_copy
            );
            registration.validate_registered_leaves(&original).unwrap();
        }
    }
    let owner = user();
    let mut control = crate::model_message_labels::internal_tool_result_envelope(
        Some(&owner),
        "secret-control",
        "private control",
        "runtime_context",
    )
    .unwrap()
    .unwrap();
    control.sensitivity = Sensitivity::Secret;
    let original = snapshot(vec![control, owner]);
    let registration =
        NativeInputPolicyRegistration::capture(&original, SOURCE_CONTRACT_REVISION, 1).unwrap();
    assert!(
        !registration
            .complete_leaf_text_policy(&original)
            .unwrap()
            .unwrap()
            .history_retention_allowed
    );
}

#[test]
fn renamed_malformed_weakened_and_cyclic_controls_remain_unresolved() {
    let mut owner = user();
    owner.retention.expires_at_unix_ms = Some(1000);
    owner.retention.delete_with_run = true;
    let control = crate::model_message_labels::internal_tool_result_envelope(
        Some(&owner),
        "control",
        "native observation",
        "runtime_context",
    )
    .unwrap()
    .unwrap();
    let another =
        model_bound_user_message("other-input".into(), "Other input".into(), destination())
            .unwrap()
            .data_envelope
            .unwrap();
    for case in 0..9 {
        let mut changed = control.clone();
        match case {
            0 => changed.provenance.source_tool_name = "model-claimed-runtime".into(),
            1 => changed.provenance.source_provider_id = "unregistered-runtime".into(),
            2 => changed.provenance.source_envelope_ids = vec!["absent-native-parent".into()],
            3 => changed.sensitivity = Sensitivity::Public,
            4 => changed.retention.expires_at_unix_ms = Some(1001),
            5 => changed.retention.delete_with_run = false,
            6 => changed
                .allowed_destinations
                .push(DestinationIdentity::Model {
                    connection_id: "other".into(),
                    connection_revision: 1,
                    model_id: "model".into(),
                    profile_revision: 1,
                }),
            7 => match &mut changed.content {
                ContentRef::Projection { projection, .. } => {
                    *projection = "forged-runtime-body".into()
                }
                ContentRef::ImmutableBlob { blob_id, .. } => {
                    *blob_id = "forged-runtime-body".into()
                }
                _ => unreachable!(),
            },
            _ => changed
                .provenance
                .source_envelope_ids
                .push(another.envelope_id.clone()),
        }
        let original = snapshot(vec![changed, owner.clone(), another.clone()]);
        let registration =
            NativeInputPolicyRegistration::capture(&original, SOURCE_CONTRACT_REVISION, 1).unwrap();
        assert!(registration.leaves[0].is_none(), "case {case}");
        assert!(
            registration
                .complete_leaf_text_policy(&original)
                .unwrap()
                .is_none()
        );
    }
    let mut first = control;
    let mut second = crate::model_message_labels::internal_tool_result_envelope(
        Some(&first),
        "second",
        "native observation",
        "context_review",
    )
    .unwrap()
    .unwrap();
    first.provenance.source_envelope_ids = vec![second.envelope_id.clone()];
    second.provenance.source_envelope_ids = vec![first.envelope_id.clone()];
    let original = snapshot(vec![first, second, owner]);
    let registration =
        NativeInputPolicyRegistration::capture(&original, SOURCE_CONTRACT_REVISION, 1).unwrap();
    assert!(registration.leaves[..2].iter().all(Option::is_none));
    let unknown =
        NativeInputPolicyRegistration::capture(&original, SOURCE_CONTRACT_REVISION + 1, 1).unwrap();
    assert!(unknown.leaves.iter().all(Option::is_none));
}

#[test]
fn native_control_registration_fits_small_stacks() {
    const STACK: &str = "LRD_TEST_CONTROL_SOURCE_STACK_BYTES";
    if let Some(bytes) = std::env::var_os(STACK) {
        let bytes: usize = bytes.to_str().unwrap().parse().unwrap();
        assert!([1024 * 1024, 2 * 1024 * 1024].contains(&bytes));
        std::thread::Builder::new()
            .name("control-source-stack".into())
            .stack_size(bytes)
            .spawn(|| {
                native_controls_require_their_actual_parents_and_never_become_independent_facts();
                native_control_chains_and_real_resume_and_delegation_producers_keep_parent_constraints();
                a_native_control_cannot_launder_unknown_private_or_secret_parent_content();
                renamed_malformed_weakened_and_cyclic_controls_remain_unresolved();
                derived::a_native_control_still_requires_the_actual_parent_archive();
            })
            .unwrap()
            .join()
            .unwrap();
        return;
    }
    for bytes in [1024 * 1024, 2 * 1024 * 1024] {
        assert!(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "native_control_registration_fits_small_stacks",
                    "--nocapture",
                    "--test-threads=1"
                ])
                .env(STACK, bytes.to_string())
                .env_remove("RUST_MIN_STACK")
                .status()
                .unwrap()
                .success()
        );
    }
}
