use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Data payload for `SignalingType::ChangeDisplaySettings` (numeric tag
/// 205). Carries the browser-requested virtual monitor mode. The desk
/// server's signaling router validates the values via
/// `desk_virtual_display::validate_mode` before forwarding to the
/// worker, and the worker replies with an updated
/// `ChangeDisplaySettingsPayload` (containing the mode the driver
/// actually applied — the IDD driver may snap to a nearby supported
/// configuration) inside a `SignalingModel::new_response`.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, ToSchema)]
pub struct ChangeDisplaySettingsPayload {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
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
        };
        let json = serde_json::to_string(&original).expect("encode");
        let back: ChangeDisplaySettingsPayload =
            serde_json::from_str(&json).expect("decode");
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
}
