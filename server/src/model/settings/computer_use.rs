//! Device-local Computer Use capability ceiling.
//!
//! Every capability defaults off. Central policy and remote clients may narrow
//! these values when computing readiness, but no signaling or manager path may
//! persist a wider value. The trusted local writer owns the optional static
//! application restriction; an empty list does not enable any capability.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(default)]
pub struct ComputerUseSettings {
    /// Monotonic revision maintained by the trusted local settings writer.
    pub revision: u64,
    /// Master local switch. `false` dominates every narrower flag.
    pub enabled: bool,
    /// Allow bounded read-only desktop/application observation.
    pub observe: bool,
    /// Allow semantic Office adapters after their independent production gate.
    pub office_semantic: bool,
    /// Allow the closed-surface macOS iWork adapters after their independent gate.
    pub iwork_semantic: bool,
    /// Allow the built-in, closed-surface browser semantic adapter after the
    /// Chrome extension has been installed and paired on this device.
    pub browser_semantic: bool,
    /// Development-only Chrome DevTools MCP adapter. It is never an automatic
    /// fallback for the extension and defaults off because Chrome requires a
    /// native approval prompt for each new debugging connection.
    pub browser_devtools_mcp: bool,
    /// Allow the reviewed Outlook (new) `mailto:` compose handoff adapter.
    /// This may create a cloud-synchronised draft, so central WriteExternalDraft
    /// authorization is still required for every exact input.
    pub communication_handoff: bool,
    /// Allow semantic desktop UI actions after their independent production gate.
    pub generic_semantic_ui: bool,
    /// Allow raw input fallback after its independent beta gate.
    pub raw_input_fallback: bool,
    /// Allow a future File Workspace Provider to create new artifacts. This is
    /// a trusted host-local ceiling only: signaling and grants may narrow it,
    /// but must never persist or widen it. No write Provider is registered by
    /// merely enabling this field.
    pub file_artifact_create: bool,
    /// Exact executable image paths whose application/window objects may be
    /// surfaced. Empty means no additional static application restriction.
    pub allowed_application_paths: Vec<String>,
    /// Exact local roots available to future file adapters. Empty means no root.
    pub allowed_file_roots: Vec<String>,
}

impl ComputerUseSettings {
    #[must_use]
    pub const fn observation_enabled(&self) -> bool {
        self.enabled && self.observe
    }

    #[must_use]
    pub fn file_artifact_create_enabled(&self) -> bool {
        self.enabled && self.file_artifact_create && !self.allowed_file_roots.is_empty()
    }

    #[must_use]
    pub const fn communication_handoff_enabled(&self) -> bool {
        self.enabled && self.communication_handoff
    }

    #[must_use]
    pub fn application_allowed(&self, image_path: &str) -> bool {
        valid_application_path(image_path)
            && (self.allowed_application_paths.is_empty()
                || self
                    .allowed_application_paths
                    .iter()
                    .any(|allowed| path_eq(allowed, image_path)))
    }
}

/// Only complete executable identities can be bound to an application object.
fn valid_application_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 4096
        && !path.chars().any(char::is_control)
        && std::path::Path::new(path).is_absolute()
        && std::path::Path::new(path).file_name().is_some()
        && !std::path::Path::new(path)
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ComputerUseApplicationPolicy {
    pub revision: u64,
    pub allowed_application_paths: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ComputerUseApplicationPolicyUpdate {
    pub expected_revision: u64,
    pub allowed_application_paths: Vec<String>,
}

impl ComputerUseSettings {
    pub fn application_policy(&self) -> ComputerUseApplicationPolicy {
        ComputerUseApplicationPolicy {
            revision: self.revision,
            allowed_application_paths: self.allowed_application_paths.clone(),
        }
    }

    #[allow(clippy::result_large_err)]
    pub fn update_application_policy(
        &mut self,
        update: ComputerUseApplicationPolicyUpdate,
    ) -> Result<(), crate::error::DeskError> {
        use crate::error::DeskError;
        use desk_utils::error::DeskErrorCode;
        if self.revision != update.expected_revision {
            return DeskError::custom_error(
                DeskErrorCode::REVISION_CONFLICT,
                "Application policy changed; reload before saving",
            );
        }
        if update.allowed_application_paths.len() > 128
            || update
                .allowed_application_paths
                .iter()
                .map(String::len)
                .sum::<usize>()
                > 32768
            || update
                .allowed_application_paths
                .iter()
                .any(|path| !valid_application_path(path))
        {
            return DeskError::custom_error(
                DeskErrorCode::INVALID_PARAMS,
                "Use at most 128 absolute executable paths, 4096 bytes per path and 32768 bytes total; control characters and parent traversal are not allowed",
            );
        }
        let revision = self.revision.checked_add(1).ok_or_else(|| {
            DeskError::new_custom_error(
                DeskErrorCode::PRECONDITION_FAILED,
                "Application policy revision exhausted",
            )
        })?;
        self.allowed_application_paths = update.allowed_application_paths;
        self.revision = revision;
        Ok(())
    }
}

#[cfg(windows)]
fn path_eq(left: &str, right: &str) -> bool {
    left.replace('/', "\\")
        .eq_ignore_ascii_case(&right.replace('/', "\\"))
}

#[cfg(not(windows))]
fn path_eq(left: &str, right: &str) -> bool {
    left == right
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_fail_closed() {
        let settings = ComputerUseSettings::default();
        assert!(!settings.observation_enabled());
        assert_eq!(settings.revision, 0);
        assert!(!settings.office_semantic);
        assert!(!settings.iwork_semantic);
        assert!(!settings.browser_semantic);
        assert!(!settings.browser_devtools_mcp);
        assert!(!settings.communication_handoff);
        assert!(!settings.generic_semantic_ui);
        assert!(!settings.raw_input_fallback);
        assert!(!settings.file_artifact_create);
        assert!(!settings.file_artifact_create_enabled());
        assert!(settings.allowed_application_paths.is_empty());
        assert!(settings.allowed_file_roots.is_empty());
        assert_eq!(
            serde_json::from_str::<ComputerUseSettings>("{}").unwrap(),
            settings
        );
    }

    #[test]
    fn master_switch_dominates_observe() {
        let mut settings = ComputerUseSettings {
            observe: true,
            ..Default::default()
        };
        assert!(!settings.observation_enabled());
        settings.enabled = true;
        assert!(settings.observation_enabled());
    }

    #[test]
    fn application_allowlist_is_exact() {
        let path = std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let settings = ComputerUseSettings {
            allowed_application_paths: vec![path.clone()],
            ..Default::default()
        };
        assert!(settings.application_allowed(&path));
        assert!(!settings.application_allowed(&format!("{path}-copy")));
    }

    #[test]
    fn optional_policy_validates_identity_and_tightening_is_immediate() {
        let path = std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let mut settings = ComputerUseSettings::default();
        assert!(settings.application_allowed(&path));
        for invalid in ["", "relative", "/", "/a/../b", "/a\nb"] {
            assert!(!settings.application_allowed(invalid));
        }
        settings
            .update_application_policy(ComputerUseApplicationPolicyUpdate {
                expected_revision: 0,
                allowed_application_paths: vec![format!("{path}-other")],
            })
            .unwrap();
        assert!(!settings.application_allowed(&path));
        assert!(!settings.observation_enabled());
        assert!(
            settings
                .update_application_policy(ComputerUseApplicationPolicyUpdate {
                    expected_revision: 0,
                    allowed_application_paths: vec![],
                })
                .is_err()
        );
        for paths in [
            vec!["relative".into()],
            vec![path.clone(); 129],
            vec!["/".repeat(4097)],
            vec![format!("/{}", "x".repeat(4095)); 9],
        ] {
            let before = settings.clone();
            assert!(
                settings
                    .update_application_policy(ComputerUseApplicationPolicyUpdate {
                        expected_revision: 1,
                        allowed_application_paths: paths,
                    })
                    .is_err()
            );
            assert_eq!(settings, before);
        }
        settings
            .update_application_policy(ComputerUseApplicationPolicyUpdate {
                expected_revision: 1,
                allowed_application_paths: vec![],
            })
            .unwrap();
        assert!(settings.application_allowed(&path));
    }

    #[test]
    fn artifact_create_requires_master_local_gate_and_an_approved_root() {
        let mut settings = ComputerUseSettings {
            file_artifact_create: true,
            ..Default::default()
        };
        assert!(!settings.file_artifact_create_enabled());
        settings.enabled = true;
        assert!(!settings.file_artifact_create_enabled());
        settings.allowed_file_roots.push(r"C:\approved".into());
        assert!(settings.file_artifact_create_enabled());
        settings.file_artifact_create = false;
        assert!(!settings.file_artifact_create_enabled());
    }
}
