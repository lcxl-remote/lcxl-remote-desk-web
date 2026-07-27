use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use wincode::{SchemaRead, SchemaWrite};

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
#[derive(
    Clone, Debug, Deserialize, Serialize, PartialEq, Eq, ToSchema, SchemaWrite, SchemaRead,
)]
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
    /// Allow file browsing (list files and inspect metadata via signaling)
    pub allow_file_browse: Option<bool>,
    /// Allow deleting files via signaling
    pub allow_file_delete: Option<bool>,
    /// Allow file transfer (upload/download via DataChannel)
    pub allow_file_transfer: Option<bool>,
    /// Timeout for security approval requests in seconds.
    /// `Some(0)` means "never time out"; `None` on the wire is normalized to the
    /// 30s default rather than treated as "never".
    pub approval_timeout: Option<u32>,
}

impl Default for SecuritySettings {
    // The neutral "unset" default: every capability prompts (`None`) and the timeout
    // is the standard 30s. This is deliberately NOT the owner's "auto-allow" posture —
    // the onboarding wizard supplies that by submitting an explicit all-`true` payload,
    // so a wizard-installed device gets owner-open defaults while a bare `/api/init`
    // (or any `..Default::default()` spread, e.g. ceiling construction) keeps the
    // restrictive all-prompt baseline. Flipping this impl to all-allow would silently
    // widen those spread-in ceilings, so owner-open lives at the wizard layer only.
    fn default() -> Self {
        Self {
            allow_remote_control: None,
            allow_clipboard_sync: None,
            allow_private_screen: None,
            allow_whiteboard: None,
            allow_terminal: None,
            allow_file_browse: None,
            allow_file_delete: None,
            allow_file_transfer: None,
            approval_timeout: Some(DEFAULT_APPROVAL_TIMEOUT_SECS),
        }
    }
}

impl SecuritySettings {
    /// A ceiling with every capability dimension left unset (`None` — "prompt"),
    /// the restrictive default for a shareable access-grant code with no explicit
    /// owner configuration. Constructed explicitly rather than via [`Default`] so
    /// it stays all-`None` independently of any future change to the global
    /// [`Default`] (which governs the host's own settings, a separate concern): a
    /// code must never silently widen to full control because the global default
    /// flipped.
    pub fn all_prompt() -> Self {
        Self {
            allow_remote_control: None,
            allow_clipboard_sync: None,
            allow_private_screen: None,
            allow_whiteboard: None,
            allow_terminal: None,
            allow_file_browse: None,
            allow_file_delete: None,
            allow_file_transfer: None,
            approval_timeout: None,
        }
    }

    /// Parse an owner-configured per-code capability ceiling from its stored JSON
    /// form (the `device_code.capabilities` column). A missing column or any parse
    /// failure yields the restrictive [`Self::all_prompt`] ceiling — a code never
    /// fails open to a wider ceiling than the owner configured.
    pub fn parse_code_ceiling(stored: Option<&str>) -> Self {
        match stored {
            Some(json) => serde_json::from_str(json).unwrap_or_else(|_| Self::all_prompt()),
            None => Self::all_prompt(),
        }
    }

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

/// Declares the capability dimensions once and derives everything that has to
/// agree with them: the permission type, the full list, the i18n keys and the
/// accessors into [`SecuritySettings`].
///
/// Keeping them in one list is a correctness requirement rather than tidiness.
/// A capability missing from the list is one whose policy changes go unnoticed
/// wherever the list drives the work — which reads downstream as "nothing
/// changed" and can leave a cached approval in force after the operator revoked
/// it. Adding a dimension here updates every dependent site at once.
macro_rules! security_capabilities {
    ($($variant:ident => $field:ident, $i18n_key:literal;)+) => {
        /// One capability dimension of [`SecuritySettings`] — the subject of a
        /// permission request and the unit that policy changes are tracked in.
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema,
            SchemaWrite, SchemaRead,
        )]
        pub enum SecurityPermissionType {
            $($variant,)+
        }

        impl SecurityPermissionType {
            /// Every capability dimension, in declaration order.
            pub const ALL: &'static [Self] = &[$(Self::$variant,)+];

            /// Returns the i18n key for this permission type
            pub fn i18n_key(&self) -> &'static str {
                match self {
                    $(Self::$variant => $i18n_key,)+
                }
            }

            /// This capability's configured value in a policy.
            pub fn read(&self, security: &SecuritySettings) -> Option<bool> {
                match self {
                    $(Self::$variant => security.$field,)+
                }
            }

            /// Overwrite this capability's configured value in a policy.
            pub fn write(&self, security: &mut SecuritySettings, value: Option<bool>) {
                match self {
                    $(Self::$variant => security.$field = value,)+
                }
            }
        }
    };
}

security_capabilities! {
    RemoteControl => allow_remote_control, "security.permission.remoteControl";
    ClipboardSync => allow_clipboard_sync, "security.permission.clipboardSync";
    PrivateScreen => allow_private_screen, "security.permission.privateScreen";
    Whiteboard => allow_whiteboard, "security.permission.whiteboard";
    Terminal => allow_terminal, "security.permission.terminal";
    FileBrowse => allow_file_browse, "security.permission.fileBrowse";
    FileDelete => allow_file_delete, "security.permission.fileDelete";
    FileTransfer => allow_file_transfer, "security.permission.fileTransfer";
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The declaration list is what makes a capability visible to everything
    /// that iterates the dimensions. A field that exists in the struct but not
    /// in the list would be configurable and yet invisible to that machinery.
    #[test]
    fn every_capability_field_is_declared() {
        let mut settings = SecuritySettings::all_prompt();
        for capability in SecurityPermissionType::ALL {
            capability.write(&mut settings, Some(true));
        }

        // Written through the list, read back off the struct: a field missing
        // from the list stays at its all-prompt `None`.
        let SecuritySettings {
            allow_remote_control,
            allow_clipboard_sync,
            allow_private_screen,
            allow_whiteboard,
            allow_terminal,
            allow_file_browse,
            allow_file_delete,
            allow_file_transfer,
            approval_timeout: _,
        } = settings;
        for (name, value) in [
            ("allow_remote_control", allow_remote_control),
            ("allow_clipboard_sync", allow_clipboard_sync),
            ("allow_private_screen", allow_private_screen),
            ("allow_whiteboard", allow_whiteboard),
            ("allow_terminal", allow_terminal),
            ("allow_file_browse", allow_file_browse),
            ("allow_file_delete", allow_file_delete),
            ("allow_file_transfer", allow_file_transfer),
        ] {
            assert_eq!(value, Some(true), "{name} is missing from the declaration");
        }
    }

    /// Reading and writing have to address the same field, or a decision about
    /// one capability would be stored against another.
    #[test]
    fn read_and_write_address_the_same_field() {
        for capability in SecurityPermissionType::ALL {
            let mut settings = SecuritySettings::all_prompt();
            capability.write(&mut settings, Some(false));

            assert_eq!(capability.read(&settings), Some(false));
            let others = SecurityPermissionType::ALL
                .iter()
                .filter(|other| *other != capability)
                .filter(|other| other.read(&settings).is_some())
                .count();
            assert_eq!(others, 0, "{capability:?} wrote another capability's field");
        }
    }

    #[test]
    fn default_approval_timeout_is_thirty_seconds() {
        assert_eq!(
            SecuritySettings::default().approval_timeout,
            Some(DEFAULT_APPROVAL_TIMEOUT_SECS)
        );
    }

    #[test]
    fn all_prompt_leaves_every_capability_unset() {
        let c = SecuritySettings::all_prompt();
        assert_eq!(c.allow_remote_control, None);
        assert_eq!(c.allow_clipboard_sync, None);
        assert_eq!(c.allow_private_screen, None);
        assert_eq!(c.allow_whiteboard, None);
        assert_eq!(c.allow_terminal, None);
        assert_eq!(c.allow_file_browse, None);
        assert_eq!(c.allow_file_delete, None);
        assert_eq!(c.allow_file_transfer, None);
        assert_eq!(c.approval_timeout, None);
    }

    #[test]
    fn parse_code_ceiling_falls_back_to_all_prompt() {
        // Missing config → all-prompt (restrictive).
        assert_eq!(
            SecuritySettings::parse_code_ceiling(None),
            SecuritySettings::all_prompt()
        );
        // Malformed JSON → all-prompt, never fails open.
        assert_eq!(
            SecuritySettings::parse_code_ceiling(Some("{not json")),
            SecuritySettings::all_prompt()
        );
        // Valid config round-trips.
        let configured = SecuritySettings {
            allow_terminal: Some(true),
            ..SecuritySettings::all_prompt()
        };
        let json = serde_json::to_string(&configured).unwrap();
        assert_eq!(
            SecuritySettings::parse_code_ceiling(Some(&json)),
            configured
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
