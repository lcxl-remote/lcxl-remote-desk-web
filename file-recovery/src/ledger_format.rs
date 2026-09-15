//! Storage format recognition is independent of the host filesystem.
use serde::{Deserialize, Deserializer, Serialize};

const CURRENT: u32 = 2;

#[derive(Debug, Serialize)]
#[serde(transparent)]
pub(super) struct Version(u32);

impl Default for Version {
    fn default() -> Self {
        // Compatibility readers normalize unversioned indices in memory. Only
        // an ordinary durable write persists the normalized representation.
        Self(CURRENT)
    }
}

impl<'de> Deserialize<'de> for Version {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = u32::deserialize(deserializer)?;
        if value != 1 && value != CURRENT {
            return Err(serde::de::Error::custom(
                "unsupported recovery index format version",
            ));
        }
        Ok(Self(CURRENT))
    }
}

#[cfg(test)]
mod tests {
    use super::super::{ChangeState, DeletedConversation, Ledger, MaterialState, Record, Scope};
    use serde_json::{Value, json};

    #[test]
    fn unversioned_index_retains_replay_and_cleanup_state() {
        let mut ledger = Ledger {
            execution_epoch: 7,
            clock_generation: 3,
            last_cleanup_at_ms: 1234,
            cleanup_clock_paused: true,
            ..Ledger::default()
        };
        let id = "a".repeat(64);
        ledger.records.insert(
            id.clone(),
            Record {
                id: id.clone(),
                scope: Scope {
                    authority: "authority".into(),
                    device: "device".into(),
                    os_user: "user".into(),
                    owner: "owner".into(),
                },
                conversation: "conversation".into(),
                operation: "operation".into(),
                generation: "generation".into(),
                file_name: "fixture.txt".into(),
                created_at_ms: 1000,
                expires_at_ms: 2000,
                bytes: 6,
                sha256: "b".repeat(64),
                change: ChangeState::OutcomeUnknown,
                material: MaterialState::Saved,
                cleanup_error: Some("fixture cleanup pending".into()),
                transaction: None,
                device_quota_pending: true,
                device_quota_managed: true,
                device_quota_settled: false,
                discard_requested: true,
                clock_generation: 3,
                storage_epoch: 6,
            },
        );
        ledger.deleted_conversations.insert(
            "fixture".into(),
            DeletedConversation {
                deleted_at_ms: 1000,
                quota_pending: true,
                storage_epoch: 6,
            },
        );
        let legacy_transaction = json!({"parent": "/fixture",
            "directory": format!(".assistant-transaction-{id}"), "parent_device": 9,
            "parent_volume_uuid": vec![7; 16], "parent_inode": 10, "directory_inode": 11,
            "original_inode": 12, "staged_inode": 13});
        ledger.records.get_mut(&id).unwrap().transaction =
            Some(serde_json::from_value(legacy_transaction.clone()).unwrap());
        let current = serde_json::to_value(&ledger).unwrap();
        for version in [None, Some(1)] {
            let mut legacy = current.clone();
            if let Some(version) = version {
                legacy["format_version"] = json!(version);
            } else {
                legacy.as_object_mut().unwrap().remove("format_version");
            }
            legacy["records"][&id]["transaction"] = legacy_transaction.clone();
            let parsed: Ledger = serde_json::from_value(legacy).unwrap();
            assert_eq!(serde_json::to_value(parsed).unwrap(), current);
        }
        assert_eq!(current["format_version"], json!(2));
    }

    #[test]
    fn unknown_or_malformed_versions_are_not_read_as_legacy() {
        for version in [
            json!(0),
            json!(3),
            json!(-1),
            json!(1.5),
            json!("1"),
            Value::Null,
            json!(4_294_967_296u64),
        ] {
            let mut value = serde_json::to_value(Ledger::default()).unwrap();
            value["format_version"] = version;
            assert!(serde_json::from_value::<Ledger>(value).is_err());
        }
    }

    #[test]
    fn version_marker_does_not_make_an_incomplete_index_valid() {
        assert!(serde_json::from_value::<Ledger>(json!({"format_version": 1})).is_err());
        let mut value = serde_json::to_value(Ledger::default()).unwrap();
        value.as_object_mut().unwrap().remove("clock_guard");
        assert!(serde_json::from_value::<Ledger>(value).is_err());
    }

    #[test]
    fn current_version_survives_repeated_serialization() {
        let bytes = serde_json::to_vec(&Ledger::default()).unwrap();
        let parsed: Ledger = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(serde_json::to_vec(&parsed).unwrap(), bytes);
        let parsed: Ledger = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(serde_json::to_vec(&parsed).unwrap(), bytes);
    }
}
