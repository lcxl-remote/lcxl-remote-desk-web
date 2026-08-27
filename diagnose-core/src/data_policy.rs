//! Conservative propagation for [`DataEnvelope`] values.
//!
//! This module deliberately provides no declassification API.  A future sink
//! authorizer may invoke a separately reviewed deterministic projector or an
//! explicit user grant, but ordinary model/provider derivation can only keep or
//! narrow labels.

use std::collections::BTreeSet;

use desk_agent_protocol::data_lineage::{
    ContentRef, DATA_ENVELOPE_SCHEMA_VERSION, DataEnvelope, DataLineageError, DataProvenance,
    DestinationIdentity, RetentionBoundary, Sensitivity, TransformationAuthority,
    TransformationRecord,
};

#[derive(Debug, Clone)]
pub struct ConservativeDerivation<'a> {
    pub output_envelope_id: &'a str,
    pub content: ContentRef,
    pub digest_sha256: &'a str,
    pub source_provider_id: &'a str,
    pub source_tool_name: &'a str,
    pub source_object_id: Option<&'a str>,
}

/// Derive one envelope from one or more validated inputs. Sensitivity is the
/// maximum, destinations are the intersection, provenance is the complete set
/// of input envelope ids, and retention is the most restrictive boundary.
pub fn derive_conservatively(
    inputs: &[DataEnvelope],
    derivation: ConservativeDerivation<'_>,
) -> Result<(DataEnvelope, TransformationRecord), DataLineageError> {
    if inputs.is_empty() {
        return Err(DataLineageError::MissingItem("input_envelopes"));
    }
    for input in inputs {
        input.validate()?;
    }

    let sensitivity = inputs
        .iter()
        .map(|input| input.sensitivity)
        .max()
        .unwrap_or(Sensitivity::Secret);

    let mut destinations: BTreeSet<DestinationIdentity> =
        inputs[0].allowed_destinations.iter().cloned().collect();
    for input in &inputs[1..] {
        let current: BTreeSet<_> = input.allowed_destinations.iter().cloned().collect();
        destinations.retain(|destination| current.contains(destination));
    }

    let retention = inputs
        .iter()
        .map(|input| input.retention)
        .reduce(RetentionBoundary::most_restrictive)
        .expect("inputs were checked non-empty");

    let source_envelope_ids: Vec<String> = inputs
        .iter()
        .map(|input| input.envelope_id.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();

    let envelope = DataEnvelope {
        schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
        envelope_id: derivation.output_envelope_id.to_string(),
        content: derivation.content,
        provenance: DataProvenance {
            source_provider_id: derivation.source_provider_id.to_string(),
            source_tool_name: derivation.source_tool_name.to_string(),
            source_object_id: derivation.source_object_id.map(str::to_string),
            source_envelope_ids: source_envelope_ids.clone(),
        },
        digest_sha256: derivation.digest_sha256.to_string(),
        sensitivity,
        allowed_destinations: destinations.into_iter().collect(),
        retention,
    };
    envelope.validate()?;

    let transformation = TransformationRecord {
        input_envelope_ids: source_envelope_ids,
        output_envelope_id: envelope.envelope_id.clone(),
        authority: TransformationAuthority::Conservative,
    };
    transformation.validate()?;
    Ok((envelope, transformation))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(character: char) -> String {
        std::iter::repeat_n(character, 64).collect()
    }

    fn model(id: &str) -> DestinationIdentity {
        DestinationIdentity::Model {
            connection_id: id.into(),
            connection_revision: 1,
            model_id: "model".into(),
            profile_revision: 1,
        }
    }

    fn input(
        id: &str,
        sensitivity: Sensitivity,
        destinations: Vec<DestinationIdentity>,
        expiry: Option<u64>,
    ) -> DataEnvelope {
        DataEnvelope {
            schema_version: DATA_ENVELOPE_SCHEMA_VERSION,
            envelope_id: id.into(),
            content: ContentRef::EphemeralObservation {
                observation_id: format!("observation-{id}"),
                size_bytes: 1,
                expires_at_unix_ms: expiry.unwrap_or(999),
            },
            provenance: DataProvenance {
                source_provider_id: "source".into(),
                source_tool_name: "read".into(),
                source_object_id: None,
                source_envelope_ids: Vec::new(),
            },
            digest_sha256: digest('a'),
            sensitivity,
            allowed_destinations: destinations,
            retention: RetentionBoundary {
                expires_at_unix_ms: expiry,
                delete_with_run: false,
            },
        }
    }

    #[test]
    fn derivation_takes_highest_sensitivity_destination_intersection_and_provenance_union() {
        let shared = model("shared");
        let left = input(
            "left",
            Sensitivity::UserContent,
            vec![shared.clone(), model("left-only")],
            Some(100),
        );
        let right = input(
            "right",
            Sensitivity::Sensitive,
            vec![shared.clone(), model("right-only")],
            Some(50),
        );
        let (derived, record) = derive_conservatively(
            &[left, right],
            ConservativeDerivation {
                output_envelope_id: "output",
                content: ContentRef::Artifact {
                    artifact_id: "artifact".into(),
                    sha256: digest('b'),
                    size_bytes: 1,
                    media_type: "application/json".into(),
                },
                digest_sha256: &digest('b'),
                source_provider_id: "report",
                source_tool_name: "summarize",
                source_object_id: None,
            },
        )
        .unwrap();

        assert_eq!(derived.sensitivity, Sensitivity::Sensitive);
        assert_eq!(derived.allowed_destinations, vec![shared]);
        assert_eq!(
            derived.provenance.source_envelope_ids,
            vec!["left".to_string(), "right".to_string()]
        );
        assert_eq!(derived.retention.expires_at_unix_ms, Some(50));
        assert_eq!(record.authority, TransformationAuthority::Conservative);
    }

    #[test]
    fn secret_and_empty_destination_set_remain_restricted() {
        let secret = input("secret", Sensitivity::Secret, Vec::new(), Some(10));
        let (derived, _) = derive_conservatively(
            &[secret],
            ConservativeDerivation {
                output_envelope_id: "output",
                content: ContentRef::EphemeralObservation {
                    observation_id: "derived".into(),
                    size_bytes: 1,
                    expires_at_unix_ms: 10,
                },
                digest_sha256: &digest('c'),
                source_provider_id: "provider",
                source_tool_name: "summarize",
                source_object_id: None,
            },
        )
        .unwrap();
        assert_eq!(derived.sensitivity, Sensitivity::Secret);
        assert!(derived.allowed_destinations.is_empty());
    }
}
