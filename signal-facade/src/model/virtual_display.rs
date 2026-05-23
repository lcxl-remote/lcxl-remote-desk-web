use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Default trailing-edge debounce window (ms) the browser waits after a
/// `resize` settles before issuing an auto `ChangeDisplaySettings`. Server
/// `VirtualDisplaySettings::Default` and `AdaptiveResolutionParams::Default`
/// both reference this constant to avoid drift.
pub const DEFAULT_ADAPTIVE_DEBOUNCE_MS: u64 = 5_000;

/// Default minimum pixel delta on either axis that the browser hook treats
/// as "significant enough to schedule a send". Below this, both width and
/// height changes are skipped to suppress micro-jitter.
pub const DEFAULT_ADAPTIVE_MIN_DELTA_PX: u32 = 16;

/// serde `skip_serializing_if` helper. The stdlib `Not::not` takes `self`
/// by value, not `&bool`, so we cannot reference it directly.
fn is_false(v: &bool) -> bool {
    !*v
}

/// Data payload for `SignalingType::ChangeDisplaySettings` (numeric tag
/// 205). Carries the browser-requested virtual monitor mode. The desk
/// server's signaling router validates the values via
/// `desk_virtual_display::validate_mode` before forwarding to the
/// worker, and the worker replies with an updated
/// `ChangeDisplaySettingsPayload` (containing the mode the driver
/// actually applied — the IDD driver may snap to a nearby supported
/// configuration) inside a `SignalingModel::new_response`.
///
/// `auto = true` marks the request as browser-initiated adaptive
/// resolution. The daemon enforces extra gates on auto requests
/// (single-client only, throttle, requires `desk_settings.adaptive_web_page_resolution`),
/// and the browser silently drops the echoed response. Manual
/// requests (`auto = false`, the default) bypass those gates and any
/// echo is treated as a normal response.
///
/// `auto` defaults to `false` and is skipped on the wire when false
/// (`skip_serializing_if`), so the JSON shape stays backward
/// compatible: a peer that only knows the old three-field schema
/// keeps working.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq, ToSchema)]
pub struct ChangeDisplaySettingsPayload {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    #[serde(default, skip_serializing_if = "is_false")]
    pub auto: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn change_display_settings_payload_serde_roundtrip() {
        let original = ChangeDisplaySettingsPayload {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
            auto: false,
        };
        let json = serde_json::to_string(&original).expect("encode");
        let back: ChangeDisplaySettingsPayload = serde_json::from_str(&json).expect("decode");
        assert_eq!(back, original);
    }

    #[test]
    fn change_display_settings_payload_accepts_browser_camel_case() {
        // The browser ships snake_case keys today (matching the rest of
        // the signaling envelope). Pin that contract so a future
        // `#[serde(rename_all = "camelCase")]` accidentally added here
        // would break browsers in flight.
        let raw = r#"{"width":1280,"height":720,"refresh_hz":60}"#;
        let p: ChangeDisplaySettingsPayload = serde_json::from_str(raw).expect("decode");
        assert_eq!(p.width, 1280);
        assert_eq!(p.height, 720);
        assert_eq!(p.refresh_hz, 60);
    }

    /// A legacy peer's JSON predates the `auto` field. `#[serde(default)]`
    /// must populate it with `false` so the daemon's auto-only gates do
    /// not fire on manual requests from older clients.
    #[test]
    fn change_display_settings_payload_omits_auto_defaults_to_false() {
        let raw = r#"{"width":1280,"height":720,"refresh_hz":60}"#;
        let p: ChangeDisplaySettingsPayload = serde_json::from_str(raw).expect("decode");
        assert!(!p.auto);
    }

    /// Auto-true requests round-trip intact, so the daemon sees the flag
    /// the browser set and applies the correct gating.
    #[test]
    fn change_display_settings_payload_roundtrip_with_auto_true() {
        let original = ChangeDisplaySettingsPayload {
            width: 1280,
            height: 720,
            refresh_hz: 60,
            auto: true,
        };
        let json = serde_json::to_string(&original).expect("encode");
        let back: ChangeDisplaySettingsPayload = serde_json::from_str(&json).expect("decode");
        assert_eq!(back, original);
        // Wire shape contains the auto field when true.
        assert!(
            json.contains("\"auto\":true"),
            "expected auto:true in JSON, got {json}"
        );
    }

    /// Response payloads constructed with `auto: false` must NOT serialise
    /// the field. This keeps the wire shape backward compatible — peers
    /// that only know the original three-field schema would never see
    /// an unexpected key, and the response stream stays terse.
    #[test]
    fn change_display_settings_payload_auto_false_skipped_from_json() {
        let p = ChangeDisplaySettingsPayload {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
            auto: false,
        };
        let json = serde_json::to_string(&p).expect("encode");
        assert!(
            !json.contains("\"auto\""),
            "auto:false must be skipped from JSON, got {json}",
        );
    }
}
