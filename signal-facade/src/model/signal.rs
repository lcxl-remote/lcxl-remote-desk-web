//! Signaling wire models grouped by responsibility.

mod envelope;
mod session;
mod signaling_type;

pub use envelope::*;
pub use session::*;
pub use signaling_type::*;
#[cfg(test)]
mod wincode_tests {
    //! Wincode `SignalingType` coverage. Every variant has an explicit
    //! `#[repr(i32)]` discriminant, and the wincode tag is
    //! locked to `i32` via `#[wincode(tag_encoding = "i32")]` so the
    //! daemon ↔ worker wire bytes use the same number the JSON wire
    //! emits (via `Serialize_repr`).
    //!
    //! Two tests cover this from different angles:
    //!
    //!   * `signaling_type_round_trips_wincode` — encode + decode each
    //!     variant and assert the decoded value matches the input. This
    //!     catches "did we forget to add `#[derive(...)]` or
    //!     `#[wincode(tag_encoding = ...)]`?" kinds of bugs.
    //!
    //!   * `signaling_type_wire_tag_matches_discriminant_for_all_variants`
    //!     — encode each variant and assert the *first four bytes* of
    //!     the encoded payload equal `(variant as i32).to_le_bytes()`.
    //!     This is the byte-level check the migration plan and code
    //!     review both call out: a round-trip test pairs encode and
    //!     decode, so a `#[wincode(tag = N)]` that silently disagrees
    //!     with the `repr(i32)` discriminant for a single variant
    //!     (e.g. typo `tag = 101` on a `= 102` variant) would still
    //!     pass round-trip — encode + decode would both use the same
    //!     wrong tag. Comparing the encoded tag directly with the
    //!     variant's `repr(i32)` discriminant catches that drift.
    use super::*;
    use strum::IntoEnumIterator as _;
    use wincode::config::{Configuration, PREALLOCATION_SIZE_LIMIT_DISABLED};

    fn unbounded_config() -> Configuration<true, PREALLOCATION_SIZE_LIMIT_DISABLED> {
        Configuration::new()
    }

    #[test]
    fn signaling_type_round_trips_wincode() {
        let config = unbounded_config();
        for variant in SignalingType::iter() {
            let bytes = wincode::config::serialize(&variant, config)
                .unwrap_or_else(|err| panic!("encode {variant:?}: {err}"));
            let back: SignalingType = wincode::config::deserialize(&bytes, config)
                .unwrap_or_else(|err| panic!("decode {variant:?}: {err}"));
            assert_eq!(
                back as i32, variant as i32,
                "round-trip mismatch for {variant:?}",
            );
        }
    }

    #[test]
    fn signaling_type_wire_tag_matches_discriminant_for_all_variants() {
        let config = unbounded_config();
        for variant in SignalingType::iter() {
            let bytes = wincode::config::serialize(&variant, config)
                .unwrap_or_else(|err| panic!("encode {variant:?}: {err}"));
            assert!(bytes.len() >= 4, "{variant:?} produced fewer than 4 bytes",);
            let tag = i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            assert_eq!(
                tag, variant as i32,
                "wincode wire tag for {variant:?} does not match its repr(i32) discriminant",
            );
        }
    }
}

#[cfg(test)]
mod init_signaling_data_tests {
    use super::*;
    use crate::model::os::OperationSystemEnum;
    use crate::model::virtual_display::{
        DEFAULT_ADAPTIVE_DEBOUNCE_MS, DEFAULT_ADAPTIVE_MIN_DELTA_PX,
    };

    /// Pre-adaptive-resolution peers ship `InitSignalingData` JSON without
    /// the three new fields. `#[serde(default)]` must populate them with
    /// sensible defaults so the daemon stays compatible with anyone still
    /// running the previous release of the signaling facade.
    #[test]
    fn init_signaling_data_legacy_json_defaults_new_fields() {
        let raw = r#"{
            "ice_servers": [],
            "user_name": "tester",
            "audio_device_list": {},
            "audio_encoder_list": [],
            "video_device_list": {},
            "video_encoder_list": [],
            "desk_settings": {},
            "is_admin": false
        }"#;
        let data: InitSignalingData = serde_json::from_str(raw).expect("decode");
        assert!(!data.virtual_display_active);
        assert_eq!(data.virtual_display_current_refresh_hz, 0);
        assert!(
            data.virtual_display_device_name.is_none(),
            "legacy peers without virtual_display_device_name must decode to None",
        );
        assert_eq!(
            data.adaptive_resolution.debounce_ms,
            DEFAULT_ADAPTIVE_DEBOUNCE_MS
        );
        assert_eq!(
            data.adaptive_resolution.min_delta_px,
            DEFAULT_ADAPTIVE_MIN_DELTA_PX
        );
        // Legacy peers predate the host-OS field; it must default to Other so
        // the browser falls back to a generic (Windows) shortcut menu rather
        // than mislabelling the host.
        assert_eq!(data.operation_system, OperationSystemEnum::Other);
    }

    /// A host that advertises its OS must round-trip so the browser can tailor
    /// host-targeted UI (e.g. macOS shortcuts) instead of assuming Windows.
    #[test]
    fn init_signaling_data_round_trips_host_os() {
        let raw = r#"{
            "ice_servers": [],
            "user_name": "tester",
            "audio_device_list": {},
            "audio_encoder_list": [],
            "video_device_list": {},
            "video_encoder_list": [],
            "desk_settings": {},
            "is_admin": false,
            "operation_system": "Mac"
        }"#;
        let data: InitSignalingData = serde_json::from_str(raw).expect("decode");
        assert_eq!(data.operation_system, OperationSystemEnum::Mac);

        let encoded = serde_json::to_string(&data).expect("encode");
        let decoded: InitSignalingData = serde_json::from_str(&encoded).expect("re-decode");
        assert_eq!(decoded.operation_system, OperationSystemEnum::Mac);
    }

    /// Empty `AdaptiveResolutionParams` JSON must fall back to the shared
    /// constants. Pin this so a future Default-by-field-init that drifts
    /// from `DEFAULT_ADAPTIVE_*` constants fails the test.
    #[test]
    fn adaptive_resolution_params_legacy_json_defaults_to_5000_16() {
        let p: AdaptiveResolutionParams = serde_json::from_str("{}").expect("decode");
        assert_eq!(p.debounce_ms, 5_000);
        assert_eq!(p.min_delta_px, 16);
        assert_eq!(p, AdaptiveResolutionParams::default());
    }
    #[test]
    fn request_remote_requires_purpose_on_the_wire() {
        let missing = serde_json::json!({"ice_servers": [], "grant_session_id": null});
        assert!(serde_json::from_value::<RequestRemoteModel>(missing).is_err());

        let model = RequestRemoteModel {
            purpose: RemoteSessionPurpose::FileManager,
            ..RequestRemoteModel::default()
        };
        let encoded = serde_json::to_value(&model).expect("encode request remote");
        assert_eq!(encoded["purpose"], "file_manager");
        let decoded: RequestRemoteModel =
            serde_json::from_value(encoded).expect("decode request remote");
        assert_eq!(decoded.purpose, RemoteSessionPurpose::FileManager);
    }
}
