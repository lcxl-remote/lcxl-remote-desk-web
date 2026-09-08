//! Content-free links between projected model inputs and their original sources.
use super::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelInputLineage {
    pub envelope_id: String,
    pub digest_sha256: String,
    pub source_provider_id: String,
    pub source_tool_name: String,
    pub source_envelope_ids: Vec<String>,
    /// True only for the canonical public system label verified before dispatch.
    pub public_system_prompt: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidModelInputLineage;

/// Inputs must be the exact envelopes returned by the model sink authorizer.
/// This records historical provenance; it grants no export or future task authority.
pub fn project_model_input_lineage(
    audit: &SinkProjectionAudit,
    inputs: &[DataEnvelope],
) -> Result<Vec<ModelInputLineage>, InvalidModelInputLineage> {
    let mut entries = Vec::with_capacity(inputs.len());
    for input in inputs {
        input.validate().map_err(|_| InvalidModelInputLineage)?;
        if !input.allowed_destinations.contains(&audit.destination) {
            return Err(InvalidModelInputLineage);
        }
        entries.push(ModelInputLineage {
            envelope_id: input.envelope_id.clone(),
            digest_sha256: input.digest_sha256.clone(),
            source_provider_id: input.provenance.source_provider_id.clone(),
            source_tool_name: input.provenance.source_tool_name.clone(),
            source_envelope_ids: input.provenance.source_envelope_ids.clone(),
            public_system_prompt: is_public_system_prompt(input),
        });
    }
    validate_model_input_lineage(audit, &entries)?;
    Ok(entries)
}

fn is_public_system_prompt(input: &DataEnvelope) -> bool {
    let Some(message_id) = input.provenance.source_object_id.as_deref() else {
        return false;
    };
    let expected_id = format!(
        "system-prompt-{}-{}",
        short_digest(message_id.as_bytes()),
        input.digest_sha256
    );
    input.envelope_id == expected_id
        && input.provenance.source_provider_id == "device-assistant-runtime"
        && input.provenance.source_tool_name == "system-prompt-projector"
        && input.provenance.source_envelope_ids.is_empty()
        && input.sensitivity == Sensitivity::Public
        && input.retention.expires_at_unix_ms.is_none()
        && !input.retention.delete_with_run
        && input.allowed_destinations.len() == 1
        && matches!(&input.content, ContentRef::ImmutableBlob {blob_id, sha256, size_bytes, media_type}
            if blob_id == &format!("system-prompt-content-{}", &input.digest_sha256[..32])
                && sha256 == &input.digest_sha256 && *size_bytes > 0
                && media_type == "text/plain;charset=utf-8")
}

/// The audit writer establishes the public marker from the complete label.
/// Readers additionally check its closed metadata shape before using it as a root.
pub fn is_audited_public_system_prompt(input: &ModelInputLineage) -> bool {
    input.public_system_prompt
        && input.digest_sha256.len() == 64
        && input
            .digest_sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
        && input.source_provider_id == "device-assistant-runtime"
        && input.source_tool_name == "system-prompt-projector"
        && input.source_envelope_ids.is_empty()
        && input
            .envelope_id
            .strip_prefix("system-prompt-")
            .is_some_and(|rest| {
                rest.split_once('-')
                    .is_some_and(|(message_hash, content_hash)| {
                        message_hash.len() == 32
                            && message_hash.bytes().all(|byte| byte.is_ascii_hexdigit())
                            && content_hash == input.digest_sha256
                    })
            })
}

pub fn validate_model_input_lineage(
    audit: &SinkProjectionAudit,
    entries: &[ModelInputLineage],
) -> Result<(), InvalidModelInputLineage> {
    if !matches!(audit.destination, DestinationIdentity::Model { .. })
        || audit.destination.validate().is_err()
        || audit.total_bytes == 0
        || audit.total_bytes > MAX_SINK_BYTES
        || entries.is_empty()
        || entries.len() > crate::sink_authorizer::MAX_SINK_ITEMS
        || entries.len() != audit.envelope_ids.len()
        || entries.len() != audit.digests_sha256.len()
    {
        return Err(InvalidModelInputLineage);
    }
    let mut seen = std::collections::BTreeMap::new();
    for ((entry, id), digest) in entries
        .iter()
        .zip(&audit.envelope_ids)
        .zip(&audit.digests_sha256)
    {
        let valid_id = |value: &str| {
            !value.trim().is_empty() && value.len() <= 512 && !value.chars().any(char::is_control)
        };
        if (entry.public_system_prompt && !is_audited_public_system_prompt(entry))
            || entry.envelope_id != *id
            || entry.digest_sha256 != *digest
            || !valid_id(id)
            || digest.len() != 64
            || !digest.bytes().all(|b| b.is_ascii_hexdigit())
            || entry
                .source_envelope_ids
                .iter()
                .any(|parent| parent == id || !valid_id(parent))
            || entry
                .source_envelope_ids
                .iter()
                .collect::<BTreeSet<_>>()
                .len()
                != entry.source_envelope_ids.len()
        {
            return Err(InvalidModelInputLineage);
        }
        DataProvenance {
            source_provider_id: entry.source_provider_id.clone(),
            source_tool_name: entry.source_tool_name.clone(),
            source_object_id: None,
            source_envelope_ids: entry.source_envelope_ids.clone(),
        }
        .validate()
        .map_err(|_| InvalidModelInputLineage)?;
        if seen
            .insert(id, entry)
            .is_some_and(|previous| previous != entry)
        {
            return Err(InvalidModelInputLineage);
        }
    }
    Ok(())
}

/// Bind an output label to the complete audited input set. This validates
/// provenance only; callers still require a durable successful provider receipt.
pub fn validate_model_output_lineage(
    audit: &SinkProjectionAudit,
    inputs: &[ModelInputLineage],
    output: &DataEnvelope,
) -> Result<(), InvalidModelInputLineage> {
    validate_model_input_lineage(audit, inputs)?;
    output.validate().map_err(|_| InvalidModelInputLineage)?;
    let expected: BTreeSet<_> = audit.envelope_ids.iter().collect();
    let actual: BTreeSet<_> = output.provenance.source_envelope_ids.iter().collect();
    if output.provenance.source_provider_id != "external-model"
        || output.provenance.source_tool_name != "model-response"
        || !output.allowed_destinations.contains(&audit.destination)
        || expected != actual
        || actual.len() != output.provenance.source_envelope_ids.len()
        || expected.contains(&output.envelope_id)
    {
        return Err(InvalidModelInputLineage);
    }
    Ok(())
}

/// Read the label only after checking the actual persisted assistant payload.
pub fn model_output_message_envelope(
    message: &ChatMessage,
) -> Result<&DataEnvelope, InvalidModelInputLineage> {
    if message.role != ChatRole::Assistant || message.image_data_url.is_some() {
        return Err(InvalidModelInputLineage);
    }
    let output = message
        .data_envelope
        .as_ref()
        .ok_or(InvalidModelInputLineage)?;
    output.validate().map_err(|_| InvalidModelInputLineage)?;
    let bytes = message_content_bytes(message).map_err(|_| InvalidModelInputLineage)?;
    if bytes.is_empty() || hex_digest(&bytes) != output.digest_sha256 {
        return Err(InvalidModelInputLineage);
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn projection() -> (SinkProjectionAudit, DataEnvelope) {
        let destination = DestinationIdentity::Model {
            connection_id: "gateway".into(),
            connection_revision: 1,
            model_id: "model".into(),
            profile_revision: 1,
        };
        let envelope = DataEnvelope {
            schema_version: 1,
            envelope_id: "exported-input".into(),
            content: ContentRef::ImmutableBlob {
                blob_id: "private-content-handle".into(),
                sha256: "a".repeat(64),
                size_bytes: 12,
                media_type: "text/plain".into(),
            },
            provenance: DataProvenance {
                source_provider_id: "device".into(),
                source_tool_name: "read-system".into(),
                source_object_id: Some("private-object-handle".into()),
                source_envelope_ids: vec!["original-read".into()],
            },
            digest_sha256: "a".repeat(64),
            sensitivity: Sensitivity::Sensitive,
            allowed_destinations: vec![destination.clone()],
            retention: RetentionBoundary {
                expires_at_unix_ms: Some(1000),
                delete_with_run: true,
            },
        };
        let audit = SinkProjectionAudit {
            destination,
            envelope_ids: vec![envelope.envelope_id.clone()],
            digests_sha256: vec![envelope.digest_sha256.clone()],
            total_bytes: 12,
        };
        (audit, envelope)
    }

    #[test]
    fn model_output_message_checks_text_and_tool_arguments() {
        for tools in [false, true] {
            let (_, mut envelope) = projection();
            let mut message = ChatMessage::text("output", ChatRole::Assistant, "report");
            if tools {
                message.tool_calls.push(ToolCallRef {
                    id: "call".into(),
                    name: "inspect".into(),
                    arguments_json: "{}".into(),
                });
            }
            let bytes = message_content_bytes(&message).unwrap();
            let digest = hex_digest(&bytes);
            envelope.digest_sha256 = digest.clone();
            envelope.content = ContentRef::ImmutableBlob {
                blob_id: "output".into(),
                sha256: digest,
                size_bytes: bytes.len() as u64,
                media_type: "application/json".into(),
            };
            message.data_envelope = Some(envelope);
            assert!(model_output_message_envelope(&message).is_ok());
            let mut changed = message.clone();
            changed.text.push_str("changed");
            assert!(model_output_message_envelope(&changed).is_err());
            if tools {
                let mut changed = message.clone();
                changed.tool_calls[0].arguments_json = "{\"target\":\"other\"}".into();
                assert!(model_output_message_envelope(&changed).is_err());
            }
            message.role = ChatRole::User;
            assert!(model_output_message_envelope(&message).is_err());
        }
    }

    #[test]
    fn model_output_lineage_requires_exact_audited_inputs_and_model_identity() {
        let (audit, input) = projection();
        let lineage = project_model_input_lineage(&audit, std::slice::from_ref(&input)).unwrap();
        let mut output = input;
        output.envelope_id = "model-output".into();
        output.provenance.source_provider_id = "external-model".into();
        output.provenance.source_tool_name = "model-response".into();
        output.provenance.source_envelope_ids = audit.envelope_ids.clone();
        assert!(validate_model_output_lineage(&audit, &lineage, &output).is_ok());
        for case in 0..7 {
            let mut bad = output.clone();
            match case {
                0 => bad.provenance.source_envelope_ids.clear(),
                1 => bad
                    .provenance
                    .source_envelope_ids
                    .push("unrecorded-input".into()),
                2 => bad
                    .provenance
                    .source_envelope_ids
                    .push("exported-input".into()),
                3 => bad.envelope_id = "exported-input".into(),
                4 => bad.provenance.source_provider_id = "untrusted-provider".into(),
                5 => bad.provenance.source_tool_name = "other-tool".into(),
                _ => bad.allowed_destinations.clear(),
            }
            assert!(validate_model_output_lineage(&audit, &lineage, &bad).is_err());
        }
    }

    #[test]
    fn model_input_lineage_preserves_export_links_without_content_handles() {
        let (audit, envelope) = projection();
        let lineage = project_model_input_lineage(&audit, &[envelope]).unwrap();
        assert_eq!(lineage[0].source_envelope_ids, ["original-read"]);
        let wire = serde_json::to_string(&lineage).unwrap();
        assert!(!wire.contains("private-content-handle"));
        assert!(!wire.contains("private-object-handle"));
        assert_eq!(
            serde_json::from_str::<Vec<ModelInputLineage>>(&wire).unwrap(),
            lineage
        );
        let mut unknown = serde_json::to_value(&lineage[0]).unwrap();
        unknown["body"] = serde_json::json!("unwanted content");
        assert!(serde_json::from_value::<ModelInputLineage>(unknown).is_err());
        // Equal duplicate inputs are unambiguous; conflicting provenance is not.
        let mut repeated = audit;
        repeated.envelope_ids.push(repeated.envelope_ids[0].clone());
        repeated
            .digests_sha256
            .push(repeated.digests_sha256[0].clone());
        let mut entries = vec![lineage[0].clone(), lineage[0].clone()];
        assert!(validate_model_input_lineage(&repeated, &entries).is_ok());
        entries[1].source_envelope_ids = vec!["different-read".into()];
        assert!(validate_model_input_lineage(&repeated, &entries).is_err());
    }

    #[test]
    fn model_input_lineage_rejects_mixed_projection_and_invalid_parents() {
        let (audit, envelope) = projection();
        let lineage = project_model_input_lineage(&audit, std::slice::from_ref(&envelope)).unwrap();
        assert!(project_model_input_lineage(&audit, &[]).is_err());
        let mut no_export = envelope.clone();
        no_export.allowed_destinations.clear();
        assert!(project_model_input_lineage(&audit, &[no_export]).is_err());
        for case in 0..6 {
            let mut wrong = lineage.clone();
            match case {
                0 => wrong[0].envelope_id = "other-export".into(),
                1 => wrong[0].digest_sha256 = "b".repeat(64),
                2 => wrong[0].source_envelope_ids = vec!["exported-input".into()],
                3 => wrong[0].source_envelope_ids = vec!["read".into(), "read".into()],
                4 => wrong[0].source_provider_id.clear(),
                _ => wrong[0].source_envelope_ids = vec!["\n".into()],
            }
            assert!(validate_model_input_lineage(&audit, &wrong).is_err());
        }
    }
}
