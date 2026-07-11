use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Default approval timeout in seconds applied when a security settings payload
/// leaves `approval_timeout` unset. A finite default (rather than "never") means
/// an unattended host does not leave inbound control requests hanging forever.
pub const DEFAULT_APPROVAL_TIMEOUT_SECS: u32 = 30;

/// Security settings for controlling remote access permissions.
///
/// Each capability field uses `Option<bool>`:
///   - `None`  — not configured (GUI: prompt user; headless: deny)
///   - `Some(true)`  — always allow
///   - `Some(false)` — always deny
///
/// `approval_timeout` is different: a missing value on the wire is normalized to
/// [`DEFAULT_APPROVAL_TIMEOUT_SECS`] (see [`SecuritySettings::normalize`]), and
/// the explicit "never" choice is persisted as the present value `Some(0)` — not
/// `None` — so it survives a save/reload round-trip (TOML omits `None`, and the
/// `serde(default)` reload would otherwise resurrect the 30s default).
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, ToSchema)]
#[serde(default)]
pub struct SecuritySettings {
    /// Allow remote desktop control (mouse/keyboard input)
    pub allow_remote_control: Option<bool>,
    /// Allow clipboard synchronization
    pub allow_clipboard_sync: Option<bool>,
    /// Allow enabling private screen mode
    pub allow_private_screen: Option<bool>,
    /// Allow whiteboard overlay
    pub allow_whiteboard: Option<bool>,
    /// Allow remote terminal access
    pub allow_terminal: Option<bool>,
    /// Allow file browsing (list/delete files via signaling)
    pub allow_file_browse: Option<bool>,
    /// Allow file transfer (upload/download via DataChannel)
    pub allow_file_transfer: Option<bool>,
    /// Timeout for security approval requests in seconds.
    /// `Some(0)` means "never time out"; `None` on the wire is normalized to the
    /// 30s default rather than treated as "never".
    pub approval_timeout: Option<u32>,
}

impl Default for SecuritySettings {
    fn default() -> Self {
        Self {
            allow_remote_control: None,
            allow_clipboard_sync: None,
            allow_private_screen: None,
            allow_whiteboard: None,
            allow_terminal: None,
            allow_file_browse: None,
            allow_file_transfer: None,
            approval_timeout: Some(DEFAULT_APPROVAL_TIMEOUT_SECS),
        }
    }
}

impl SecuritySettings {
    /// Normalize an unset `approval_timeout` (`None`) to the finite default.
    ///
    /// Under the current semantics only the explicit present value `Some(0)`
    /// means "never"; a `None` arriving from a client that omits the field must
    /// not be interpreted as "never", so it collapses to
    /// [`DEFAULT_APPROVAL_TIMEOUT_SECS`]. Capability fields are left untouched —
    /// `None` there legitimately means "prompt the user".
    pub fn normalize(&mut self) {
        if self.approval_timeout.is_none() {
            self.approval_timeout = Some(DEFAULT_APPROVAL_TIMEOUT_SECS);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_approval_timeout_is_thirty_seconds() {
        assert_eq!(
            SecuritySettings::default().approval_timeout,
            Some(DEFAULT_APPROVAL_TIMEOUT_SECS)
        );
    }

    #[test]
    fn normalize_promotes_none_timeout_to_default() {
        let mut settings = SecuritySettings {
            approval_timeout: None,
            ..SecuritySettings::default()
        };
        settings.normalize();
        assert_eq!(
            settings.approval_timeout,
            Some(DEFAULT_APPROVAL_TIMEOUT_SECS)
        );
    }

    #[test]
    fn normalize_preserves_explicit_never() {
        let mut settings = SecuritySettings {
            approval_timeout: Some(0),
            ..SecuritySettings::default()
        };
        settings.normalize();
        // "never" is a present Some(0) and must not be rewritten to the default.
        assert_eq!(settings.approval_timeout, Some(0));
    }

    #[test]
    fn normalize_leaves_capability_fields_untouched() {
        let mut settings = SecuritySettings {
            allow_remote_control: None,
            allow_terminal: Some(true),
            approval_timeout: Some(60),
            ..SecuritySettings::default()
        };
        settings.normalize();
        assert_eq!(settings.allow_remote_control, None);
        assert_eq!(settings.allow_terminal, Some(true));
        assert_eq!(settings.approval_timeout, Some(60));
    }

    #[test]
    fn explicit_never_survives_toml_round_trip() {
        // "never" persisted as Some(0) must reload as Some(0), not fall back to
        // the 30s default the way an omitted (None) field would.
        let settings = SecuritySettings {
            approval_timeout: Some(0),
            ..SecuritySettings::default()
        };
        let serialized = toml::to_string(&settings).expect("serialize");
        assert!(
            serialized.contains("approval_timeout = 0"),
            "expected present zero in TOML, got:\n{serialized}"
        );
        let reloaded: SecuritySettings = toml::from_str(&serialized).expect("deserialize");
        assert_eq!(reloaded.approval_timeout, Some(0));
    }
}
