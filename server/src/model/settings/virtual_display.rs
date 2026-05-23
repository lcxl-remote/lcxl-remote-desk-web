use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Virtual display (Windows IDD) settings. Pulled out into its own
/// section because the knob is system-level (only the
/// `ServiceDaemon` startup mode acts on it, and changing it requires
/// the `LcxlVirtualDisplay` driver to already be staged) — not a
/// per-session capture setting. Kept as a struct rather than a bare
/// `bool` so future fields (e.g. exclusive-mode toggle, pre-detach
/// prompt duration) slot in without another schema migration.
#[derive(
    Clone, Debug, Default, Deserialize, Serialize, ToSchema, PartialEq, Eq,
)]
#[serde(default)]
pub struct VirtualDisplaySettings {
    /// Whether the daemon should create the Windows IDD virtual
    /// monitor at startup. Service-daemon mode only — other startup
    /// modes ignore the flag entirely. The `/api/desk/settings/virtual-display`
    /// POST endpoint rejects `enabled = true` (with body code
    /// `PRECONDITION_FAILED`) unless the driver is already staged.
    pub enabled: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Default must keep the daemon's virtual display off so a fresh
    /// install does not unexpectedly create an IDD on first boot.
    #[test]
    fn virtual_display_settings_default_disabled() {
        let s = VirtualDisplaySettings::default();
        assert!(!s.enabled);
    }

    /// TOML deserialised from an empty section populates the field
    /// with its type default (`false`), thanks to `#[serde(default)]`
    /// at the struct level.
    #[test]
    fn virtual_display_settings_empty_toml_defaults_to_disabled() {
        let s: VirtualDisplaySettings = toml::from_str("").expect("decode");
        assert!(!s.enabled);
    }

    /// `enabled = true` round-trips through JSON intact.
    #[test]
    fn virtual_display_settings_json_roundtrip_true() {
        let s = VirtualDisplaySettings { enabled: true };
        let json = serde_json::to_string(&s).expect("encode");
        let back: VirtualDisplaySettings = serde_json::from_str(&json).expect("decode");
        assert!(back.enabled);
    }

    /// `enabled = true` round-trips through TOML intact (the on-disk
    /// format the daemon actually reads).
    #[test]
    fn virtual_display_settings_toml_roundtrip_true() {
        let s = VirtualDisplaySettings { enabled: true };
        let toml_str = toml::to_string(&s).expect("encode");
        let back: VirtualDisplaySettings = toml::from_str(&toml_str).expect("decode");
        assert!(back.enabled);
    }
}
