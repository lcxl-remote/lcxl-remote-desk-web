//! Edge-side gate on what evidence may leave this machine for an AI model.
//!
//! In the thin-edge model the diagnose orchestrator runs centrally; the edge
//! (this desk server) only provides read-only evidence on request. This policy
//! is the edge's **final say** over what may leave the machine — it is applied
//! locally on every collection regardless of who asked. It is deliberately
//! separate from [`super::AiModelSettings`] (provider / model / key), which on a
//! manager-attached edge is only meaningful for the self-contained `mcp-stdio`
//! mode: a thin edge needs the gate but no model credentials.
//!
//! Both flags default to `false` (most restrictive / fail-closed), so a config
//! written before this section existed — or one where the operator never opted
//! in — never lets logs or screenshots leave the host.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Persisted edge collection policy. Mirrors the runtime
/// [`desk_diagnose_core::selection::CollectionPolicy`] gate, read live at
/// collection time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(default)]
pub struct CollectionPolicySettings {
    /// Whether logs (`log.recent`, `container.logs`, raw `container.inspect`) may
    /// be collected and sent to a model. Default `false`.
    pub allow_logs: bool,
    /// Whether a screenshot may be collected and sent to a model. Default
    /// `false`. A screenshot additionally requires the per-request
    /// `include_screen` flag.
    pub allow_screen: bool,
}

impl CollectionPolicySettings {
    /// Project the masked public view returned by the query endpoint. The policy
    /// carries no secret, so the public view is the value itself.
    pub fn public_view(&self) -> CollectionPolicySettings {
        *self
    }

    /// Apply an update in place. Every field uses `None` = leave unchanged.
    pub fn apply_update(&mut self, update: CollectionPolicySettingsUpdate) {
        if let Some(allow_logs) = update.allow_logs {
            self.allow_logs = allow_logs;
        }
        if let Some(allow_screen) = update.allow_screen {
            self.allow_screen = allow_screen;
        }
    }
}

/// Update body for `POST /api/desk/settings/collection-policy`. Every field is
/// optional: `None` leaves the stored value unchanged.
#[derive(Clone, Debug, Default, Serialize, Deserialize, ToSchema)]
pub struct CollectionPolicySettingsUpdate {
    pub allow_logs: Option<bool>,
    pub allow_screen: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both flags default to false (fail-closed) and a config without the section
    /// deserializes to that default.
    #[test]
    fn defaults_are_fail_closed() {
        let p = CollectionPolicySettings::default();
        assert!(!p.allow_logs);
        assert!(!p.allow_screen);
        let parsed: CollectionPolicySettings = serde_json::from_str("{}").expect("empty config");
        assert_eq!(parsed, CollectionPolicySettings::default());
    }

    /// Update semantics: `Some` sets, `None` leaves unchanged.
    #[test]
    fn update_sets_some_leaves_none() {
        let mut p = CollectionPolicySettings::default();
        p.apply_update(CollectionPolicySettingsUpdate {
            allow_logs: Some(true),
            allow_screen: None,
        });
        assert!(p.allow_logs);
        assert!(!p.allow_screen);

        p.apply_update(CollectionPolicySettingsUpdate {
            allow_logs: None,
            allow_screen: Some(true),
        });
        assert!(p.allow_logs); // untouched
        assert!(p.allow_screen);

        // An all-None update changes nothing.
        p.apply_update(CollectionPolicySettingsUpdate::default());
        assert!(p.allow_logs);
        assert!(p.allow_screen);
    }
}
