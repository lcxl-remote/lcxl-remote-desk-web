use desk_signal_facade::model::virtual_display::{
    DEFAULT_ADAPTIVE_DEBOUNCE_MS, DEFAULT_ADAPTIVE_MIN_DELTA_PX,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Default daemon-side rate limit (ms) between accepted auto
/// `ChangeDisplaySettings` requests. Defensive against a buggy / runaway
/// client that bypasses the browser-side debounce. Should be much smaller
/// than `DEFAULT_ADAPTIVE_DEBOUNCE_MS` so a single well-behaved client
/// never trips it.
pub const DEFAULT_ADAPTIVE_THROTTLE_MS: u64 = 1_000;

/// Virtual display (Windows IDD) settings. Pulled out into its own
/// section because the knob is system-level (only the
/// `ServiceDaemon` startup mode acts on it, and changing it requires
/// the `LcxlVirtualDisplay` driver to already be staged) — not a
/// per-session capture setting. Kept as a struct rather than a bare
/// `bool` so future fields (e.g. exclusive-mode toggle, pre-detach
/// prompt duration) slot in without another schema migration.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema, PartialEq, Eq)]
#[serde(default)]
pub struct VirtualDisplaySettings {
    /// Whether the daemon should create the Windows IDD virtual
    /// monitor at startup. Service-daemon mode only — other startup
    /// modes ignore the flag entirely. The `/api/desk/settings/virtual-display`
    /// POST endpoint rejects `enabled = true` (with body code
    /// `PRECONDITION_FAILED`) unless the driver is already staged.
    pub enabled: bool,
    /// Browser-side trailing-edge debounce window (ms) for the adaptive
    /// resolution hook. Resize events within this window reset the
    /// timer; the send fires only after the wrapper has been stable for
    /// this many ms. Sourced from `config.toml`; ferried to the browser
    /// via `InitSignalingData::adaptive_resolution`.
    pub adaptive_debounce_ms: u64,
    /// Daemon-side minimum interval (ms) between accepted auto
    /// ChangeDisplaySettings requests. `0` is allowed and disables the
    /// defense entirely.
    pub adaptive_throttle_ms: u64,
    /// Minimum pixel delta on either axis the browser hook treats as
    /// significant. Below this, both width and height changes are
    /// skipped to suppress micro-jitter from cursor-driven resize loops.
    pub adaptive_min_delta_px: u32,
}

impl Default for VirtualDisplaySettings {
    fn default() -> Self {
        Self {
            enabled: false,
            adaptive_debounce_ms: DEFAULT_ADAPTIVE_DEBOUNCE_MS,
            adaptive_throttle_ms: DEFAULT_ADAPTIVE_THROTTLE_MS,
            adaptive_min_delta_px: DEFAULT_ADAPTIVE_MIN_DELTA_PX,
        }
    }
}

impl VirtualDisplaySettings {
    /// Lowest debounce honoured. Below this the browser would issue an
    /// effectively per-frame ChangeDisplaySettings storm — there is no
    /// useful operator scenario for that.
    const DEBOUNCE_MIN_MS: u64 = 100;
    /// Upper bound on debounce. Browser `setTimeout` accepts up to
    /// 2^31-1 ms, but anything more than an hour is almost certainly a
    /// config typo. Clamp loudly so the operator notices.
    const DEBOUNCE_MAX_MS: u64 = 3_600_000;
    /// Throttle floor — `0` is explicitly allowed (means "no defense"),
    /// so the minimum is the same value as the floor.
    const THROTTLE_MIN_MS: u64 = 0;
    /// Throttle ceiling. A minute-long throttle is already a runaway
    /// configuration; anything higher is a typo.
    const THROTTLE_MAX_MS: u64 = 60_000;
    /// Delta floor must be at least 1 px — `0` would re-trigger the
    /// hook on every layout event, defeating the purpose.
    const DELTA_MIN_PX: u32 = 1;
    /// Delta ceiling caps the value at "no auto change unless the
    /// browser viewport jumps by ~half a 1080p height" — beyond this is
    /// almost certainly a typo. 1024 px is generous enough not to
    /// surprise legitimate operator preferences.
    const DELTA_MAX_PX: u32 = 1024;

    /// Clamp out-of-range values that may arrive from a hand-edited
    /// `config.toml`. Called from `Settings::new` after deserialisation.
    /// Emits a warn-level log per clamped field so the operator sees
    /// the adjustment in the daemon log on next boot.
    pub fn sanitize(&mut self) {
        clamp_u64(
            &mut self.adaptive_debounce_ms,
            Self::DEBOUNCE_MIN_MS,
            Self::DEBOUNCE_MAX_MS,
            "adaptive_debounce_ms",
        );
        clamp_u64(
            &mut self.adaptive_throttle_ms,
            Self::THROTTLE_MIN_MS,
            Self::THROTTLE_MAX_MS,
            "adaptive_throttle_ms",
        );
        let mut delta = u64::from(self.adaptive_min_delta_px);
        clamp_u64(
            &mut delta,
            u64::from(Self::DELTA_MIN_PX),
            u64::from(Self::DELTA_MAX_PX),
            "adaptive_min_delta_px",
        );
        // Safe: the lo / hi bounds fit in u32 by construction.
        self.adaptive_min_delta_px = delta as u32;
    }
}

fn clamp_u64(v: &mut u64, lo: u64, hi: u64, name: &str) {
    if *v < lo || *v > hi {
        log::warn!(
            "[virtual-display] {name}={} out of [{lo},{hi}], clamping",
            *v
        );
        *v = (*v).clamp(lo, hi);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Default must keep the daemon's virtual display off so a fresh
    /// install does not unexpectedly create an IDD on first boot, AND
    /// the three new adaptive knobs default to the shared constants.
    #[test]
    fn virtual_display_settings_default_matches_expected() {
        let s = VirtualDisplaySettings::default();
        assert!(!s.enabled);
        assert_eq!(s.adaptive_debounce_ms, 5_000);
        assert_eq!(s.adaptive_throttle_ms, 1_000);
        assert_eq!(s.adaptive_min_delta_px, 16);
    }

    /// TOML deserialised from an empty section populates each field with
    /// its type default, thanks to `#[serde(default)]` at the struct
    /// level and the manual `Default` impl using the shared constants.
    #[test]
    fn virtual_display_settings_empty_toml_defaults_to_disabled() {
        let s: VirtualDisplaySettings = toml::from_str("").expect("decode");
        assert_eq!(s, VirtualDisplaySettings::default());
    }

    /// Pre-adaptive-config toml only carries the `enabled` field. The
    /// three new fields must reach their defaults via `#[serde(default)]`
    /// — this is the upgrade path for an existing host that did not have
    /// adaptive_* in their config.toml.
    #[test]
    fn virtual_display_settings_legacy_toml_only_enabled_field_keeps_defaults() {
        let s: VirtualDisplaySettings = toml::from_str("enabled = true").expect("decode");
        assert!(s.enabled);
        assert_eq!(s.adaptive_debounce_ms, 5_000);
        assert_eq!(s.adaptive_throttle_ms, 1_000);
        assert_eq!(s.adaptive_min_delta_px, 16);
    }

    /// `enabled = true` round-trips through JSON intact, plus the three
    /// new adaptive fields keep their defaults.
    #[test]
    fn virtual_display_settings_json_roundtrip_true() {
        let s = VirtualDisplaySettings {
            enabled: true,
            ..Default::default()
        };
        let json = serde_json::to_string(&s).expect("encode");
        let back: VirtualDisplaySettings = serde_json::from_str(&json).expect("decode");
        assert_eq!(back, s);
    }

    /// All four fields round-trip through TOML — the on-disk format the
    /// daemon actually reads.
    #[test]
    fn virtual_display_settings_toml_roundtrip_true() {
        let s = VirtualDisplaySettings {
            enabled: true,
            adaptive_debounce_ms: 4_321,
            adaptive_throttle_ms: 567,
            adaptive_min_delta_px: 32,
        };
        let toml_str = toml::to_string(&s).expect("encode");
        let back: VirtualDisplaySettings = toml::from_str(&toml_str).expect("decode");
        assert_eq!(back, s);
    }

    /// Below the floor — `0` is the most common "I disabled it" mistake
    /// for the debounce knob and would produce a per-resize signaling
    /// storm. Clamp it to `DEBOUNCE_MIN_MS`.
    #[test]
    fn sanitize_clamps_debounce_below_minimum() {
        let mut s = VirtualDisplaySettings {
            adaptive_debounce_ms: 0,
            ..Default::default()
        };
        s.sanitize();
        assert_eq!(s.adaptive_debounce_ms, 100);
    }

    /// Above the ceiling — typoed `86_400_000` (24h) gets pulled back to
    /// 1h. The other fields are untouched.
    #[test]
    fn sanitize_clamps_debounce_above_maximum() {
        let mut s = VirtualDisplaySettings {
            adaptive_debounce_ms: 86_400_000,
            ..Default::default()
        };
        s.sanitize();
        assert_eq!(s.adaptive_debounce_ms, 3_600_000);
        // Untouched defaults remain.
        assert_eq!(s.adaptive_throttle_ms, 1_000);
        assert_eq!(s.adaptive_min_delta_px, 16);
    }

    /// `throttle = 0` is explicitly allowed (operator opt-out of the
    /// daemon-side defense). It must not be clamped.
    #[test]
    fn sanitize_allows_throttle_zero() {
        let mut s = VirtualDisplaySettings {
            adaptive_throttle_ms: 0,
            ..Default::default()
        };
        s.sanitize();
        assert_eq!(s.adaptive_throttle_ms, 0);
    }

    /// `delta = 0` would re-trigger on every layout event. Clamp to 1.
    #[test]
    fn sanitize_clamps_delta_zero_to_one() {
        let mut s = VirtualDisplaySettings {
            adaptive_min_delta_px: 0,
            ..Default::default()
        };
        s.sanitize();
        assert_eq!(s.adaptive_min_delta_px, 1);
    }

    /// In-range values pass through untouched. This nails the contract
    /// that `sanitize` is idempotent on healthy configs.
    #[test]
    fn sanitize_leaves_in_range_values_untouched() {
        let original = VirtualDisplaySettings {
            enabled: true,
            adaptive_debounce_ms: 2_500,
            adaptive_throttle_ms: 500,
            adaptive_min_delta_px: 8,
        };
        let mut s = original.clone();
        s.sanitize();
        assert_eq!(s, original);
    }
}
