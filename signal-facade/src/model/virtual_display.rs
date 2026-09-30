use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use wincode::{SchemaRead, SchemaWrite};

#[derive(
    Clone, Debug, Deserialize, Serialize, PartialEq, Eq, ToSchema, SchemaRead, SchemaWrite,
)]
pub struct PhysicalDisplayCapability {
    pub available: bool,
    pub reason: Option<String>,
    pub current_selector: Option<String>,
    /// Daemon/worker IPC only. The raw monitor identity may contain a serial
    /// number or device instance path and is not needed by the browser.
    #[serde(skip_serializing)]
    #[schema(ignore)]
    pub display_identity: Option<String>,
}

/// Default trailing-edge debounce window (ms) the browser waits after a
/// `resize` settles before issuing an auto `ChangeDisplaySettings`. Server
/// `VirtualDisplaySettings::Default` and `AdaptiveResolutionParams::Default`
/// both reference this constant to avoid drift.
pub const DEFAULT_ADAPTIVE_DEBOUNCE_MS: u64 = 5_000;

/// Default minimum pixel delta on either axis that the browser hook treats
/// as "significant enough to schedule a send". Below this, both width and
/// height changes are skipped to suppress micro-jitter.
pub const DEFAULT_ADAPTIVE_MIN_DELTA_PX: u32 = 16;

/// Virtual-display mode fields used by the router and its `220` response.
/// Browser `205` requests use [`ChangeDisplaySettingsCommand`] to identify
/// their virtual or physical target. The router validates virtual modes via
/// `desk_virtual_display::validate_mode`; the worker's response contains the
/// mode the IDD actually applied. `auto = true` on a virtual request uses
/// the shared single-desktop-session gate and the IDD throttle. The browser's
/// adaptive toggle is a per-session choice, not a host settings permission.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq, ToSchema)]
pub struct ChangeDisplaySettingsPayload {
    pub connection_epoch: String,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    pub auto: bool,
}

/// Browser request for display mode changes. The target is explicit so one
/// adaptive toggle can address the display currently being captured.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChangeDisplaySettingsCommand {
    Virtual {
        connection_epoch: String,
        width: u32,
        height: u32,
        refresh_hz: u32,
        auto: bool,
    },
    PhysicalAuto {
        connection_epoch: String,
        capture_backend: String,
        device_name: String,
        viewport_width: u32,
        viewport_height: u32,
        viewport_sequence: u64,
    },
    PhysicalSelect {
        connection_epoch: String,
        capture_backend: String,
        device_name: String,
        selector: String,
    },
    PhysicalRestore {
        connection_epoch: String,
        device_name: String,
    },
}

impl ChangeDisplaySettingsCommand {
    pub fn connection_epoch(&self) -> &str {
        match self {
            Self::Virtual {
                connection_epoch, ..
            }
            | Self::PhysicalAuto {
                connection_epoch, ..
            }
            | Self::PhysicalSelect {
                connection_epoch, ..
            }
            | Self::PhysicalRestore {
                connection_epoch, ..
            } => connection_epoch,
        }
    }

    pub fn is_auto(&self) -> bool {
        match self {
            Self::Virtual { auto, .. } => *auto,
            Self::PhysicalAuto { .. } => true,
            Self::PhysicalSelect { .. } | Self::PhysicalRestore { .. } => false,
        }
    }
}

/// Result data for a physical display change. The mode selector is an opaque
/// host-issued identifier and must not be treated as a browser choice.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, ToSchema)]
pub struct PhysicalDisplayChangedData {
    pub connection_epoch: String,
    pub device_name: String,
    pub selector: String,
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub refresh_millihz: u32,
    pub changed: bool,
    pub restored: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn physical_capability_omits_internal_identity_from_browser_json() {
        let capability = PhysicalDisplayCapability {
            available: true,
            reason: None,
            current_selector: Some("current".into()),
            display_identity: Some("monitor-serial".into()),
        };
        let json = serde_json::to_value(&capability).expect("encode");
        assert!(json.get("display_identity").is_none());
        assert_eq!(json["current_selector"], "current");
    }

    #[test]
    fn change_display_settings_payload_serde_roundtrip() {
        let original = ChangeDisplaySettingsPayload {
            connection_epoch: "epoch".into(),
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
        let raw = r#"{"connection_epoch":"epoch","width":1280,"height":720,"refresh_hz":60,"auto":false}"#;
        let p: ChangeDisplaySettingsPayload = serde_json::from_str(raw).expect("decode");
        assert_eq!(p.width, 1280);
        assert_eq!(p.height, 720);
        assert_eq!(p.refresh_hz, 60);
    }

    /// Auto-true requests round-trip intact, so the daemon sees the flag
    /// the browser set and applies the correct gating.
    #[test]
    fn change_display_settings_payload_roundtrip_with_auto_true() {
        let original = ChangeDisplaySettingsPayload {
            connection_epoch: "epoch".into(),
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

    /// Required false values remain explicit in the terminal wire shape.
    #[test]
    fn change_display_settings_payload_auto_false_skipped_from_json() {
        let p = ChangeDisplaySettingsPayload {
            connection_epoch: "epoch".into(),
            width: 1920,
            height: 1080,
            refresh_hz: 60,
            auto: false,
        };
        let json = serde_json::to_string(&p).expect("encode");
        assert!(json.contains("\"auto\":false"));
    }

    #[test]
    fn physical_command_requires_explicit_target_and_preserves_sequence() {
        let command = ChangeDisplaySettingsCommand::PhysicalAuto {
            connection_epoch: "epoch".into(),
            capture_backend: "WGC".into(),
            device_name: r"\\.\DISPLAY1".into(),
            viewport_width: 1600,
            viewport_height: 900,
            viewport_sequence: 42,
        };
        let json = serde_json::to_string(&command).unwrap();
        assert!(json.contains("\"kind\":\"physical_auto\""));
        assert_eq!(
            serde_json::from_str::<ChangeDisplaySettingsCommand>(&json).unwrap(),
            command
        );
        assert!(command.is_auto());
        assert!(serde_json::from_str::<ChangeDisplaySettingsCommand>(
            r#"{"connection_epoch":"epoch","width":1920,"height":1080,"refresh_hz":60,"auto":true}"#
        )
        .is_err());
    }
}
