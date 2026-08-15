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
    //!     This byte-level check is necessary because a round-trip test
    //!     pairs encode and
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

    #[test]
    fn signaling_type_discriminants_are_unique() {
        let mut tags: Vec<i32> = SignalingType::iter()
            .map(|variant| variant as i32)
            .collect();
        let total = tags.len();
        tags.sort_unstable();
        tags.dedup();
        assert_eq!(tags.len(), total, "duplicate SignalingType discriminant");
    }
}

#[cfg(test)]
mod remote_access_initialized_data_tests {
    use super::*;
    use crate::model::os::OperationSystemEnum;
    use crate::model::virtual_display::{
        DEFAULT_ADAPTIVE_DEBOUNCE_MS, DEFAULT_ADAPTIVE_MIN_DELTA_PX,
    };

    /// Optional display metadata keeps sensible defaults. Terminal session
    /// settings and the connection epoch remain required below.
    #[test]
    fn remote_access_initialized_data_defaults_optional_display_fields() {
        let raw = r#"{
            "ice_servers": [],
            "user_name": "tester",
            "audio_device_list": {},
            "audio_encoder_list": [],
            "video_device_list": {},
            "video_encoder_list": [],
            "suggested_session_settings": {
                "capture_audio": false,
                "image_capture": null,
                "video_device_name": null,
                "show_mouse": true,
                "video_encoder": null,
                "video_quality": 22,
                "video_fps": 60,
                "enable_dirty_rect": true,
                "adaptive_bitrate": true,
                "audio_capture": null,
                "audio_device": null,
                "audio_encoder": null
            },
            "session_settings_capabilities": {
                "capture_audio": "unsupported",
                "image_capture": "apply",
                "video_device_name": "apply",
                "show_mouse": "apply",
                "video_encoder": "apply",
                "video_quality": "apply",
                "video_fps": "apply",
                "enable_dirty_rect": "apply",
                "adaptive_bitrate": "apply",
                "audio_capture": "unsupported",
                "audio_device": "unsupported",
                "audio_encoder": "unsupported"
            },
            "connection_epoch": "epoch-test",
            "is_admin": false
        }"#;
        let data: RemoteAccessInitializedData = serde_json::from_str(raw).expect("decode");
        assert!(!data.virtual_display_active);
        assert_eq!(data.virtual_display_current_refresh_hz, 0);
        assert!(
            data.virtual_display_device_name.is_none(),
            "missing virtual_display_device_name must decode to None",
        );
        assert_eq!(
            data.adaptive_resolution.debounce_ms,
            DEFAULT_ADAPTIVE_DEBOUNCE_MS
        );
        assert_eq!(
            data.adaptive_resolution.min_delta_px,
            DEFAULT_ADAPTIVE_MIN_DELTA_PX
        );
        // Missing host OS defaults to Other so
        // the browser falls back to a generic (Windows) shortcut menu rather
        // than mislabelling the host.
        assert_eq!(data.operation_system, OperationSystemEnum::Other);
        assert!(
            data.video_encoder_capabilities.is_empty(),
            "an omitted optional encoder capability list remains empty"
        );
    }

    /// A host that advertises its OS must round-trip so the browser can tailor
    /// host-targeted UI (e.g. macOS shortcuts) instead of assuming Windows.
    #[test]
    fn remote_access_initialized_data_round_trips_host_os() {
        let raw = r#"{
            "ice_servers": [],
            "user_name": "tester",
            "audio_device_list": {},
            "audio_encoder_list": [],
            "video_device_list": {},
            "video_encoder_list": [],
            "suggested_session_settings": {
                "capture_audio": false,
                "image_capture": null,
                "video_device_name": null,
                "show_mouse": true,
                "video_encoder": null,
                "video_quality": 22,
                "video_fps": 60,
                "enable_dirty_rect": true,
                "adaptive_bitrate": true,
                "audio_capture": null,
                "audio_device": null,
                "audio_encoder": null
            },
            "session_settings_capabilities": {
                "capture_audio": "unsupported",
                "image_capture": "apply",
                "video_device_name": "apply",
                "show_mouse": "apply",
                "video_encoder": "apply",
                "video_quality": "apply",
                "video_fps": "apply",
                "enable_dirty_rect": "apply",
                "adaptive_bitrate": "apply",
                "audio_capture": "unsupported",
                "audio_device": "unsupported",
                "audio_encoder": "unsupported"
            },
            "connection_epoch": "epoch-test",
            "is_admin": false,
            "operation_system": "Mac"
        }"#;
        let data: RemoteAccessInitializedData = serde_json::from_str(raw).expect("decode");
        assert_eq!(data.operation_system, OperationSystemEnum::Mac);

        let encoded = serde_json::to_string(&data).expect("encode");
        let decoded: RemoteAccessInitializedData =
            serde_json::from_str(&encoded).expect("re-decode");
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
