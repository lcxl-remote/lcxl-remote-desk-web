use super::*;
use crate::model_message_labels::model_bound_user_message;
use desk_agent_protocol::data_lineage::{ContentRef, MAX_SOURCE_LINEAGE_BYTES, Sensitivity};
use sha2::{Digest, Sha256};

mod export;

fn fixture() -> (SinkProjectionAudit, Vec<DataEnvelope>) {
    let destination = DestinationIdentity::Model {
        connection_id: "gateway".into(),
        connection_revision: 1,
        model_id: "model".into(),
        profile_revision: 1,
    };
    let inputs: Vec<_> = ["first", "second"]
        .iter()
        .map(|id| {
            model_bound_user_message(
                (*id).into(),
                "private prompt bytes absent from audit".into(),
                destination.clone(),
            )
            .unwrap()
            .data_envelope
            .unwrap()
        })
        .collect();
    let audit = SinkProjectionAudit {
        destination,
        envelope_ids: inputs
            .iter()
            .map(|input| input.envelope_id.clone())
            .collect(),
        total_bytes: 74,
    };
    (audit, inputs)
}

#[test]
fn snapshot_round_trips_original_labels_without_body_and_hashes_all_metadata() {
    let (audit, inputs) = fixture();
    let snapshot = NativeModelInputSnapshot::capture(&audit, &inputs).unwrap();
    let lineage = project_model_input_lineage(&audit, &inputs).unwrap();
    snapshot.validate(&audit, &lineage).unwrap();
    let encoded = serde_json::to_vec(&snapshot).unwrap();
    let decoded: NativeModelInputSnapshot = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(decoded, snapshot);
    assert_eq!(decoded.envelopes, inputs);
    assert_eq!(
        decoded.fingerprint().unwrap(),
        format!("{:x}", Sha256::digest(&encoded))
    );
    assert!(
        !String::from_utf8(encoded)
            .unwrap()
            .contains("private prompt bytes")
    );
    let mut unknown = serde_json::to_value(&snapshot).unwrap();
    unknown["body"] = "private prompt".into();
    assert!(serde_json::from_value::<NativeModelInputSnapshot>(unknown).is_err());
}

#[test]
fn constraints_missing_from_thin_lineage_change_the_original_snapshot_fingerprint() {
    let (audit, inputs) = fixture();
    let original = NativeModelInputSnapshot::capture(&audit, &inputs).unwrap();
    let lineage = project_model_input_lineage(&audit, &inputs).unwrap();
    for case in 0..7 {
        let mut changed = inputs.clone();
        match case {
            0 => changed[0].sensitivity = Sensitivity::Sensitive,
            1 => changed[0].retention.expires_at_unix_ms = Some(1000),
            2 => changed[0].retention.delete_with_run = !changed[0].retention.delete_with_run,
            3 => changed[0]
                .allowed_destinations
                .push(DestinationIdentity::Model {
                    connection_id: "other".into(),
                    connection_revision: 1,
                    model_id: "other-model".into(),
                    profile_revision: 1,
                }),
            4 => changed[0].provenance.source_object_id = Some("other-source-object".into()),
            5 => {
                let ContentRef::ImmutableBlob { blob_id, .. } = &mut changed[0].content else {
                    panic!("fixture blob")
                };
                *blob_id = "other-immutable-reference".into();
            }
            _ => {
                let ContentRef::ImmutableBlob { media_type, .. } = &mut changed[0].content else {
                    panic!("fixture blob")
                };
                *media_type = "application/json".into();
            }
        }
        assert_eq!(
            project_model_input_lineage(&audit, &changed).unwrap(),
            lineage
        );
        let snapshot = NativeModelInputSnapshot::capture(&audit, &changed).unwrap();
        assert_ne!(
            snapshot.fingerprint().unwrap(),
            original.fingerprint().unwrap(),
            "case {case}"
        );
    }
}

#[test]
fn capture_rejects_a_different_sink_set_order_or_non_model_recipient() {
    let (audit, inputs) = fixture();
    let snapshot = NativeModelInputSnapshot::capture(&audit, &inputs).unwrap();
    let lineage = project_model_input_lineage(&audit, &inputs).unwrap();
    for case in 0..5 {
        let mut changed = audit.clone();
        match case {
            0 => changed.envelope_ids.swap(0, 1),
            1 => {
                changed.envelope_ids.pop();
            }
            2 => changed.envelope_ids[0] = "other-envelope".into(),
            3 => {
                changed.destination = DestinationIdentity::EmailAccount {
                    account_id: "mail".into(),
                }
            }
            _ => changed.total_bytes = 0,
        }
        assert!(NativeModelInputSnapshot::capture(&changed, &inputs).is_err());
        assert!(snapshot.validate(&changed, &lineage).is_err());
    }
    let mut invalid = snapshot;
    invalid.envelopes[0].allowed_destinations.clear();
    assert!(invalid.fingerprint().is_err());
}

#[test]
fn bounded_long_history_keeps_all_entries_and_checks_the_last_label() {
    let (mut audit, inputs) = fixture();
    let mut long = vec![inputs[0].clone(); 300];
    audit.envelope_ids = long.iter().map(|input| input.envelope_id.clone()).collect();
    audit.total_bytes = 11_100;
    let snapshot = NativeModelInputSnapshot::capture(&audit, &long).unwrap();
    assert_eq!(snapshot.envelopes.len(), 300);
    long[299].digest_sha256 = "a".repeat(64);
    assert!(NativeModelInputSnapshot::capture(&audit, &long).is_err());
    // The limit applies to encoded metadata, not only each individual label.
    let oversized = vec![inputs[0].clone(); MAX_SOURCE_LINEAGE_BYTES / 128];
    assert!(NativeModelInputSnapshot::capture(&audit, &oversized).is_err());
}
