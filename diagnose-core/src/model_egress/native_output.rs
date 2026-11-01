//! Metadata-only evidence for one accepted native model output. It grants no
//! current source access, export, task authority or permission to repeat I/O.

use super::{ModelInputLineage, NativeModelInputSnapshot, validate_model_output_lineage};
use crate::sink_authorizer::SinkProjectionAudit;
use desk_agent_protocol::{
    ai_assistant::source::validate_source_id,
    data_lineage::{DataEnvelope, DestinationIdentity, MAX_SOURCE_LINEAGE_BYTES},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{self, Write};

/// Returned only after an authenticated host reads the original successful
/// receipt. Source-policy and native-task authorization remain separate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeModelOutputSources {
    pub evidence: NativeModelOutputEvidence,
    pub inputs: Vec<ModelInputLineage>,
    pub input_snapshot: NativeModelInputSnapshot,
}

impl NativeModelOutputSources {
    pub fn validate(&self, output: &DataEnvelope) -> Result<(), &'static str> {
        self.evidence.validate_sources(&self.inputs, output)?;
        let audit = self.input_snapshot.audit_with_lineage(&self.inputs)?;
        self.input_snapshot
            .validate_export_authorization(&self.evidence.model_export_authorization_id)?;
        validate_model_output_lineage(&audit, &self.inputs, output)
            .map_err(|_| "native output changed its complete original input set")?;
        if self.evidence.destination != self.input_snapshot.destination
            || self.evidence.input_snapshot_sha256
                != super::sink_metadata_sha256(&self.input_snapshot)?
        {
            return Err("native output replaced its original full input labels");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeModelOutputEvidence {
    pub physical_call_id: String,
    pub context_conversation_id: String,
    pub model_export_authorization_id: String,
    pub destination: DestinationIdentity,
    pub output_envelope_id: String,
    pub output_digest_sha256: String,
    pub output_label_sha256: String,
    pub input_lineage_sha256: String,
    pub input_snapshot_sha256: String,
    pub provider_started_at_unix_ms: u64,
    pub completed_at_unix_ms: u64,
}

impl NativeModelOutputEvidence {
    /// The host first verifies the original durable completed/accepted receipt
    /// and its indexed output identity. This checks its full output label and
    /// lineage without treating a historical observation as a current grant.
    #[expect(
        clippy::too_many_arguments,
        reason = "Keep immutable native receipt identity and exact audited source bindings explicit"
    )]
    pub fn from_receipt(
        call: &str,
        conversation: &str,
        export: &str,
        started: u64,
        completed: u64,
        audit: &SinkProjectionAudit,
        inputs: &[ModelInputLineage],
        snapshot: &NativeModelInputSnapshot,
        output: &DataEnvelope,
    ) -> Result<Self, &'static str> {
        snapshot.validate(audit, inputs)?;
        snapshot.validate_export_authorization(export)?;
        validate_model_output_lineage(audit, inputs, output)
            .map_err(|_| "native output does not match its audited inputs")?;
        let evidence = Self {
            physical_call_id: call.into(),
            context_conversation_id: conversation.into(),
            model_export_authorization_id: export.into(),
            destination: audit.destination.clone(),
            output_envelope_id: output.envelope_id.clone(),
            output_digest_sha256: output.digest_sha256.clone(),
            output_label_sha256: model_output_label_sha256(output)?,
            input_lineage_sha256: model_input_lineage_sha256(inputs)?,
            input_snapshot_sha256: snapshot.fingerprint()?,
            provider_started_at_unix_ms: started,
            completed_at_unix_ms: completed,
        };
        evidence.validate()?;
        Ok(evidence)
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        for id in [
            &self.physical_call_id,
            &self.context_conversation_id,
            &self.model_export_authorization_id,
            &self.output_envelope_id,
        ] {
            validate_source_id(id)?;
        }
        self.destination
            .validate()
            .map_err(|_| "invalid native model evidence destination")?;
        if !matches!(self.destination, DestinationIdentity::Model { .. })
            || [
                &self.output_digest_sha256,
                &self.output_label_sha256,
                &self.input_lineage_sha256,
                &self.input_snapshot_sha256,
            ]
            .iter()
            .any(|hash| hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
            || self.provider_started_at_unix_ms == 0
            || self.completed_at_unix_ms < self.provider_started_at_unix_ms
            || self.completed_at_unix_ms > i64::MAX as u64
        {
            return Err("invalid native model output evidence");
        }
        Ok(())
    }

    fn validate_sources(
        &self,
        inputs: &[ModelInputLineage],
        output: &DataEnvelope,
    ) -> Result<(), &'static str> {
        self.validate()?;
        if self.output_envelope_id != output.envelope_id
            || self.output_digest_sha256 != output.digest_sha256
            || !output.allowed_destinations.contains(&self.destination)
            || self.output_label_sha256 != model_output_label_sha256(output)?
            || self.input_lineage_sha256 != model_input_lineage_sha256(inputs)?
        {
            return Err("native model output evidence changed");
        }
        Ok(())
    }
}

pub fn model_output_label_sha256(output: &DataEnvelope) -> Result<String, &'static str> {
    output
        .validate()
        .map_err(|_| "invalid native model output label")?;
    sink_metadata_sha256(output)
}

pub fn model_input_lineage_sha256(inputs: &[ModelInputLineage]) -> Result<String, &'static str> {
    if inputs.is_empty() {
        return Err("empty native model input lineage");
    }
    sink_metadata_sha256(inputs)
}

/// Hash bounded metadata while serializing; do not allocate an oversized JSON
/// buffer merely to discover that it exceeds the lineage byte limit.
pub fn sink_metadata_sha256<T: Serialize + ?Sized>(value: &T) -> Result<String, &'static str> {
    sink_metadata_hash_and_size(value).map(|(hash, _)| hash)
}

/// Exact canonical JSON size under the same streaming metadata bound. This
/// permits incremental DAG accounting without reserializing its growing prefix.
pub fn sink_metadata_size<T: Serialize + ?Sized>(value: &T) -> Result<usize, &'static str> {
    sink_metadata_hash_and_size(value).map(|(_, size)| size)
}

fn sink_metadata_hash_and_size<T: Serialize + ?Sized>(
    value: &T,
) -> Result<(String, usize), &'static str> {
    let mut writer = DigestWriter {
        hash: Sha256::new(),
        bytes: 0,
    };
    serde_json::to_writer(&mut writer, value)
        .map_err(|_| "native model lineage metadata exceeds its byte bound or is invalid")?;
    Ok((format!("{:x}", writer.hash.finalize()), writer.bytes))
}

struct DigestWriter {
    hash: Sha256,
    bytes: usize,
}

impl Write for DigestWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let next = self
            .bytes
            .checked_add(bytes.len())
            .filter(|value| *value <= MAX_SOURCE_LINEAGE_BYTES)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "model lineage metadata limit")
            })?;
        self.hash.update(bytes);
        self.bytes = next;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streaming_metadata_size_counts_utf8_escaping_and_rejects_the_same_oversized_encoding_as_its_hash()
     {
        let value = vec!["雪", "quote\"", "new\nline", "back\\slash"];
        assert_eq!(
            sink_metadata_size(&value).unwrap(),
            serde_json::to_vec(&value).unwrap().len()
        );
        let escaped = "\n".repeat(MAX_SOURCE_LINEAGE_BYTES / 2);
        assert!(escaped.len() < MAX_SOURCE_LINEAGE_BYTES);
        assert!(sink_metadata_size(&escaped).is_err());
        assert!(sink_metadata_sha256(&escaped).is_err());
    }
    use crate::{
        model_egress::project_model_input_lineage, model_message_labels::model_bound_user_message,
    };
    use desk_agent_protocol::data_lineage::{
        ContentRef, DataProvenance, RetentionBoundary, Sensitivity,
    };

    fn fixture() -> (
        SinkProjectionAudit,
        Vec<ModelInputLineage>,
        DataEnvelope,
        NativeModelInputSnapshot,
    ) {
        let destination = DestinationIdentity::Model {
            connection_id: "gateway".into(),
            connection_revision: 1,
            model_id: "model".into(),
            profile_revision: 1,
        };
        let input = model_bound_user_message("input".into(), "request".into(), destination.clone())
            .unwrap()
            .data_envelope
            .unwrap();
        let audit = SinkProjectionAudit {
            destination: destination.clone(),
            envelope_ids: vec![input.envelope_id.clone()],
            digests_sha256: vec![input.digest_sha256.clone()],
            total_bytes: 7,
        };
        let snapshot =
            NativeModelInputSnapshot::capture(&audit, std::slice::from_ref(&input)).unwrap();
        let inputs = project_model_input_lineage(&audit, &[input]).unwrap();
        let hash = format!("{:x}", Sha256::digest(b"done"));
        let output = DataEnvelope {
            schema_version: 1,
            envelope_id: "accepted-output".into(),
            content: ContentRef::ImmutableBlob {
                blob_id: "output-body".into(),
                sha256: hash.clone(),
                size_bytes: 4,
                media_type: "text/plain".into(),
            },
            provenance: DataProvenance {
                source_provider_id: "external-model".into(),
                source_tool_name: "model-response".into(),
                source_object_id: None,
                source_envelope_ids: audit.envelope_ids.clone(),
            },
            digest_sha256: hash,
            sensitivity: Sensitivity::UserContent,
            allowed_destinations: vec![destination],
            retention: RetentionBoundary {
                expires_at_unix_ms: None,
                delete_with_run: true,
            },
        };
        (audit, inputs, output, snapshot)
    }

    #[test]
    fn evidence_binds_the_whole_label_and_input_lineage_in_addition_to_body_identity() {
        let (audit, inputs, output, snapshot) = fixture();
        let evidence = NativeModelOutputEvidence::from_receipt(
            "physical",
            "conversation",
            "export",
            10,
            20,
            &audit,
            &inputs,
            &snapshot,
            &output,
        )
        .unwrap();
        evidence.validate_sources(&inputs, &output).unwrap();
        for case in 0..4 {
            let mut changed = output.clone();
            match case {
                0 => changed.sensitivity = Sensitivity::Public,
                1 => changed.retention.expires_at_unix_ms = Some(1000),
                2 => changed
                    .allowed_destinations
                    .push(DestinationIdentity::Model {
                        connection_id: "another-gateway".into(),
                        connection_revision: 1,
                        model_id: "model".into(),
                        profile_revision: 1,
                    }),
                _ => changed.provenance.source_object_id = Some("different-object".into()),
            }
            assert_eq!(changed.envelope_id, output.envelope_id);
            assert_eq!(changed.digest_sha256, output.digest_sha256);
            assert!(evidence.validate_sources(&inputs, &changed).is_err());
        }
        let mut changed_inputs = inputs.clone();
        changed_inputs[0]
            .source_envelope_ids
            .push("unrecorded-parent".into());
        assert!(evidence.validate_sources(&changed_inputs, &output).is_err());
        let mut changed = evidence;
        changed.completed_at_unix_ms = 9;
        assert!(changed.validate().is_err());
    }

    #[test]
    fn a_label_without_the_original_complete_input_set_cannot_form_receipt_evidence() {
        let (audit, inputs, mut output, snapshot) = fixture();
        output.provenance.source_envelope_ids.clear();
        assert!(
            NativeModelOutputEvidence::from_receipt(
                "physical",
                "conversation",
                "export",
                10,
                20,
                &audit,
                &inputs,
                &snapshot,
                &output
            )
            .is_err()
        );
    }

    #[test]
    fn output_evidence_rejects_replaced_original_constraints_even_when_thin_lineage_matches() {
        let (audit, inputs, output, snapshot) = fixture();
        let evidence = NativeModelOutputEvidence::from_receipt(
            "physical",
            "conversation",
            "export",
            10,
            20,
            &audit,
            &inputs,
            &snapshot,
            &output,
        )
        .unwrap();
        let original = NativeModelOutputSources {
            evidence,
            inputs,
            input_snapshot: snapshot,
        };
        original.validate(&output).unwrap();
        for case in 0..5 {
            let mut replaced = original.clone();
            let input = &mut replaced.input_snapshot.envelopes[0];
            match case {
                0 => input.sensitivity = Sensitivity::Sensitive,
                1 => input.retention.expires_at_unix_ms = Some(1000),
                2 => input.retention.delete_with_run = !input.retention.delete_with_run,
                3 => input.provenance.source_object_id = Some("other-source-object".into()),
                _ => input.allowed_destinations.push(DestinationIdentity::Model {
                    connection_id: "other".into(),
                    connection_revision: 1,
                    model_id: "other-model".into(),
                    profile_revision: 1,
                }),
            }
            assert_eq!(
                project_model_input_lineage(&audit, &replaced.input_snapshot.envelopes).unwrap(),
                original.inputs
            );
            assert!(replaced.validate(&output).is_err(), "case {case}");
        }
        let mut omitted = original.clone();
        omitted.input_snapshot.envelopes.clear();
        assert!(omitted.validate(&output).is_err());
        let mut wrong_sink = original;
        wrong_sink.input_snapshot.total_bytes += 1;
        assert!(wrong_sink.validate(&output).is_err());
    }

    #[test]
    fn metadata_hash_matches_canonical_json_and_rejects_overflow_while_streaming() {
        let (audit, inputs, output, _) = fixture();
        let encoded = serde_json::to_vec(&inputs).unwrap();
        assert_eq!(
            model_input_lineage_sha256(&inputs).unwrap(),
            format!("{:x}", Sha256::digest(&encoded))
        );
        assert_eq!(
            model_output_label_sha256(&output).unwrap(),
            format!("{:x}", Sha256::digest(serde_json::to_vec(&output).unwrap()))
        );
        assert!(model_input_lineage_sha256(&[]).is_err());
        assert!(sink_metadata_sha256(&"x".repeat(MAX_SOURCE_LINEAGE_BYTES)).is_err());
        // Repeated transport entries are hashed in full rather than imposing a
        // new message-count limit on an otherwise bounded native audit.
        let repeated = vec![inputs[0].clone(); 300];
        assert!(model_input_lineage_sha256(&repeated).is_ok());
        assert!(!audit.envelope_ids.is_empty());
    }
}
