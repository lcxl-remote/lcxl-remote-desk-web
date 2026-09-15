use super::{InodeIdentity, Transaction, TransactionIdentity, WindowsIdentity};
use serde_json::{Value, json};

fn legacy() -> Value {
    json!({"parent": "/fixture", "directory": ".assistant-transaction-fixture",
        "parent_device": 9, "parent_inode": 10, "directory_inode": 11,
        "original_inode": 12, "staged_inode": 13})
}

#[test]
fn legacy_macos_volume_identity_survives_decoding_on_any_host() {
    let mut value = legacy();
    value["parent_volume_uuid"] = serde_json::to_value([7; 16]).unwrap();
    let tx: Transaction = serde_json::from_value(value).unwrap();
    let TransactionIdentity::Macos { volume_uuid, files } = &tx.identity else {
        panic!("macOS identity was lost");
    };
    assert_eq!(*volume_uuid, [7; 16]);
    assert_eq!(
        *files,
        InodeIdentity {
            parent_device: 9,
            parent_inode: 10,
            directory_inode: Some(11),
            original_inode: 12,
            staged_inode: Some(13)
        }
    );
    let encoded = serde_json::to_value(&tx).unwrap();
    assert_eq!(encoded["identity"]["platform"], "macos");
    assert!(encoded.get("parent_inode").is_none());
    let reopened: Transaction = serde_json::from_value(encoded).unwrap();
    assert_eq!(reopened.identity, tx.identity);
    assert_eq!(
        tx.identity.validate_host().is_ok(),
        cfg!(target_os = "macos")
    );
}

#[test]
fn legacy_without_volume_uuid_is_not_promoted_to_macos_or_windows() {
    let tx: Transaction = serde_json::from_value(legacy()).unwrap();
    assert!(matches!(tx.identity, TransactionIdentity::Unix { .. }));
    assert_eq!(
        tx.identity.validate_host().is_ok(),
        cfg!(all(unix, not(target_os = "macos")))
    );
}

#[test]
fn windows_roundtrip_preserves_high_64_bits_and_all_object_ids() {
    let mut first = [1u8; 16];
    let mut second = first;
    second[15] = 2;
    first[8] = 3;
    let tx = Transaction {
        parent: r"C:\fixture".into(),
        directory: ".assistant-transaction-fixture".into(),
        identity: TransactionIdentity::Windows {
            files: WindowsIdentity {
                volume_serial: u64::MAX,
                parent_file_id: first,
                directory_file_id: Some(second),
                original_file_id: [4; 16],
                staged_file_id: Some([5; 16]),
            },
        },
    };
    let encoded = serde_json::to_vec(&tx).unwrap();
    let reopened: Transaction = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(reopened.identity, tx.identity);
    assert_ne!(first, second);
    assert_eq!(reopened.identity.validate_host().is_ok(), cfg!(windows));
    let mut malformed = serde_json::to_value(&tx).unwrap();
    malformed["identity"]["files"]["parent_file_id"] = serde_json::to_value([1; 8]).unwrap();
    assert!(serde_json::from_value::<Transaction>(malformed).is_err());
}

#[test]
fn unknown_or_mixed_identity_cannot_fall_back_to_legacy_decoding() {
    let tx: Transaction = serde_json::from_value(legacy()).unwrap();
    let mut value = serde_json::to_value(&tx).unwrap();
    value["identity"]["platform"] = json!("future");
    assert!(serde_json::from_value::<Transaction>(value).is_err());
    let mut mixed = legacy();
    mixed["identity"] = serde_json::to_value(tx.identity).unwrap();
    assert!(serde_json::from_value::<Transaction>(mixed).is_err());
    let mut truncated = legacy();
    truncated["parent_volume_uuid"] = serde_json::to_value([0; 8]).unwrap();
    assert!(serde_json::from_value::<Transaction>(truncated).is_err());
}

#[test]
fn legacy_ledger_versions_write_the_normalized_format() {
    let tx: Transaction = serde_json::from_value(legacy()).unwrap();
    for version in [None, Some(1), Some(2)] {
        let mut value = serde_json::to_value(super::Ledger::default()).unwrap();
        if let Some(version) = version {
            value["format_version"] = json!(version);
        } else {
            value.as_object_mut().unwrap().remove("format_version");
        }
        let parsed: super::Ledger = serde_json::from_value(value).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap()["format_version"], 2);
        // The transaction compatibility reader is host independent too.
        let reopened: Transaction =
            serde_json::from_value(serde_json::to_value(&tx).unwrap()).unwrap();
        assert_eq!(reopened.identity, tx.identity);
    }
}
