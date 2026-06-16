//! AI model gateway configuration with a hard secret boundary.
//!
//! [`AiModelSettings`] is a **top-level** field of [`crate::model::settings::Settings`]
//! (sibling of `turn` / `log` / `security`), deliberately **not** part of
//! [`crate::model::settings::SystemSettings`]. That placement keeps the model
//! `api_key` out of the legacy `GET /api/desk/settings` response (which returns
//! `settings.system`) and out of `RemoteSystemSettings` (a subset of
//! `SystemSettings`), so the secret can never leak through those paths by
//! construction.
//!
//! The secret boundary has three faces:
//! - [`AiModelSettings`] — persisted form. `api_key` is plaintext on disk but
//!   its [`Debug`] is redacted, and the full-config save log must not print it
//!   (see `Settings::save`).
//! - [`AiModelSettingsPublic`] — what `GET` returns. It reports only whether a
//!   key is configured (`api_key_set`), never the key itself.
//! - [`AiModelSettingsUpdate`] — what `POST` accepts. `api_key` is write-only
//!   with explicit semantics (leave / clear / set).

use std::fmt;

use desk_agent_protocol::ExecutionMode;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Whether an [`ExecutionMode`] is one of the three the M2 confirm-execute flow
/// supports. `SessionApproved` / `Automated` are frozen in the protocol enum but
/// not selectable yet (they need the M4 policy engine); persisting them is
/// rejected so the stored mode stays in the usable set.
fn is_m2_selectable(mode: ExecutionMode) -> bool {
    matches!(
        mode,
        ExecutionMode::SuggestOnly | ExecutionMode::ReadOnly | ExecutionMode::ConfirmEachAction
    )
}

/// How the model gateway is asked to constrain its output format.
///
/// The diagnosis parser degrades gracefully regardless of this setting, so it is
/// purely an enforcement hint to the gateway. Pick `json_schema` only when the
/// gateway is known to *enforce* it (some gateways silently ignore unknown
/// `response_format` types; others reject the request).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResponseFormatMode {
    /// No `response_format` is sent; the model may return prose (the parser then
    /// degrades to a low-confidence fallback).
    None,
    /// Request syntactically valid JSON (`{"type":"json_object"}`). Broadly
    /// supported; the default.
    #[default]
    JsonObject,
    /// Request the diagnosis JSON schema (`{"type":"json_schema",...}`), locking
    /// the shape + enums in addition to JSON validity.
    JsonSchema,
}

/// Where the model call is dialed from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum GatewayMode {
    /// The server dials the model gateway directly using the locally configured
    /// model, base URL, and API key. Default.
    #[default]
    Direct,
    /// Route the model call through the manager proxy: the server sends only the
    /// prompt (authenticated with its `manager_api_token`) and the manager
    /// injects the provider credentials and relays the response. Requires the
    /// manager URL and token to be configured; no local provider key is needed.
    ManagerProxy,
}

/// Persisted AI model gateway configuration.
///
/// `Debug` is implemented by hand so `api_key` is never rendered; the derived
/// `Debug` would leak it (and `Settings` derives `Debug`, propagating to here).
#[derive(Clone, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct AiModelSettings {
    /// Provider identifier, e.g. `"openai-compatible"`.
    pub provider: Option<String>,
    /// Model name passed to the provider.
    pub model: Option<String>,
    /// Base URL of the (OpenAI-compatible) chat completions endpoint.
    pub base_url: Option<String>,
    /// Server-side secret. Never serialized into a public / remote view; its
    /// `Debug` is redacted and it must be stripped from full-config logs.
    pub api_key: Option<String>,
    /// Upper bound on the structured evidence sent to the model, in bytes.
    pub max_context_bytes: Option<u64>,
    /// How the gateway is asked to constrain output format. Default
    /// `json_object`.
    pub response_format: ResponseFormatMode,
    /// How far the AI may go in acting on the device. Default `suggest_only`
    /// (the AI only proposes commands). `read_only` / `confirm_each_action`
    /// permit confirmed execution of whitelist templates; every real execution
    /// still requires an explicit per-command user approval. `session_approved`
    /// additionally lets the first approval of a template stand for the rest of
    /// the connection's session. `automated` (run without any confirmation) is
    /// not implemented and is refused.
    pub execution_mode: ExecutionMode,
    /// Where the model call is dialed from. Default `direct` (dial the gateway
    /// locally). `manager_proxy` routes the call through the manager, which
    /// injects the provider credentials (see [`GatewayMode`]).
    pub gateway_mode: GatewayMode,
}

impl AiModelSettings {
    /// Whether a non-empty API key is configured.
    pub fn api_key_set(&self) -> bool {
        self.api_key.as_deref().is_some_and(|k| !k.is_empty())
    }

    /// Whether the gateway has the minimum fields needed to attempt a call:
    /// `model`, `base_url`, and `api_key` all present and non-empty. Mirrors the
    /// precondition the model layer enforces before dialing the gateway, and
    /// gates the AI agent / diagnosis signaling routes (configuring the gateway
    /// is the operator opt-in).
    pub fn is_configured(&self) -> bool {
        let nonempty = |o: &Option<String>| o.as_deref().is_some_and(|v| !v.is_empty());
        nonempty(&self.model) && nonempty(&self.base_url) && self.api_key_set()
    }

    /// Project the masked public view returned by the query endpoint.
    pub fn public_view(&self) -> AiModelSettingsPublic {
        AiModelSettingsPublic {
            provider: self.provider.clone(),
            model: self.model.clone(),
            base_url: self.base_url.clone(),
            max_context_bytes: self.max_context_bytes,
            response_format: self.response_format,
            execution_mode: self.execution_mode,
            gateway_mode: self.gateway_mode,
            api_key_set: self.api_key_set(),
        }
    }

    /// Apply an update in place. Every field uses `None` = leave unchanged;
    /// `api_key` additionally treats `Some("")` as clear (set to `None`) and
    /// `Some(non-empty)` as set.
    pub fn apply_update(&mut self, update: AiModelSettingsUpdate) {
        if let Some(provider) = update.provider {
            self.provider = Some(provider);
        }
        if let Some(model) = update.model {
            self.model = Some(model);
        }
        if let Some(base_url) = update.base_url {
            self.base_url = Some(base_url);
        }
        if let Some(max_context_bytes) = update.max_context_bytes {
            self.max_context_bytes = Some(max_context_bytes);
        }
        if let Some(response_format) = update.response_format {
            self.response_format = response_format;
        }
        // Reject the not-yet-selectable modes so the persisted value stays in
        // the M2-usable set; a `None` leaves the stored mode unchanged.
        if let Some(execution_mode) = update.execution_mode
            && is_m2_selectable(execution_mode)
        {
            self.execution_mode = execution_mode;
        }
        if let Some(gateway_mode) = update.gateway_mode {
            self.gateway_mode = gateway_mode;
        }
        match update.api_key {
            None => {}                                          // leave unchanged
            Some(key) if key.is_empty() => self.api_key = None, // clear
            Some(key) => self.api_key = Some(key),              // set
        }
    }
}

impl fmt::Debug for AiModelSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AiModelSettings")
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("base_url", &self.base_url)
            // Redact: report presence, never the value.
            .field("api_key", &self.api_key.as_ref().map(|_| "***"))
            .field("max_context_bytes", &self.max_context_bytes)
            .field("response_format", &self.response_format)
            .field("execution_mode", &self.execution_mode)
            .field("gateway_mode", &self.gateway_mode)
            .finish()
    }
}

/// Masked public view returned by `GET /api/desk/settings/ai-model`.
///
/// Carries no secret: only whether a key is configured (`api_key_set`).
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema, Default)]
pub struct AiModelSettingsPublic {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub max_context_bytes: Option<u64>,
    pub response_format: ResponseFormatMode,
    pub execution_mode: ExecutionMode,
    pub gateway_mode: GatewayMode,
    /// Whether a non-empty API key is configured. The key itself is never
    /// returned.
    pub api_key_set: bool,
}

/// Update body for `POST /api/desk/settings/ai-model`.
///
/// Every field is optional: `None` leaves the stored value unchanged. `api_key`
/// is write-only with three-way semantics (see [`AiModelSettings::apply_update`]).
#[derive(Clone, Serialize, Deserialize, ToSchema, Default)]
pub struct AiModelSettingsUpdate {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub max_context_bytes: Option<u64>,
    /// `None` leaves the stored mode unchanged.
    pub response_format: Option<ResponseFormatMode>,
    /// `None` leaves the stored mode unchanged. A not-yet-selectable mode
    /// (`session_approved` / `automated`) is ignored.
    pub execution_mode: Option<ExecutionMode>,
    /// `None` leaves the stored gateway mode unchanged.
    pub gateway_mode: Option<GatewayMode>,
    /// Write-only. `None` = leave unchanged; `Some("")` = clear; `Some(x)` = set.
    pub api_key: Option<String>,
}

impl fmt::Debug for AiModelSettingsUpdate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AiModelSettingsUpdate")
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("base_url", &self.base_url)
            .field("max_context_bytes", &self.max_context_bytes)
            .field("response_format", &self.response_format)
            .field("execution_mode", &self.execution_mode)
            .field("gateway_mode", &self.gateway_mode)
            .field("api_key", &self.api_key.as_ref().map(|_| "***"))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn configured() -> AiModelSettings {
        AiModelSettings {
            provider: Some("openai-compatible".into()),
            model: Some("example-model".into()),
            base_url: Some("https://api.example/v1".into()),
            api_key: Some("sk-secret-value".into()),
            max_context_bytes: Some(131_072),
            response_format: ResponseFormatMode::JsonObject,
            execution_mode: ExecutionMode::SuggestOnly,
            gateway_mode: GatewayMode::Direct,
        }
    }

    /// The public view never carries the key, only `api_key_set`.
    #[test]
    fn public_view_masks_the_key() {
        let public = configured().public_view();
        assert!(public.api_key_set);
        let json = serde_json::to_string(&public).expect("serialize public");
        assert!(
            !json.contains("sk-secret-value"),
            "public view leaked the key: {json}"
        );
        assert!(
            !json.contains("api_key\""),
            "public view must not carry an api_key field: {json}"
        );
    }

    /// No configured key reports `api_key_set = false`; an empty string counts
    /// as unset.
    #[test]
    fn api_key_set_treats_empty_as_unset() {
        let mut s = AiModelSettings::default();
        assert!(!s.api_key_set());
        s.api_key = Some(String::new());
        assert!(!s.api_key_set());
        s.api_key = Some("k".into());
        assert!(s.api_key_set());
    }

    /// Update semantics: `None` leaves the key, `Some("")` clears it,
    /// `Some(x)` sets it. Non-secret fields with `None` are unchanged.
    #[test]
    fn update_api_key_three_way_semantics() {
        let mut s = configured();

        // None leaves everything unchanged.
        s.apply_update(AiModelSettingsUpdate::default());
        assert_eq!(s.api_key.as_deref(), Some("sk-secret-value"));
        assert_eq!(s.model.as_deref(), Some("example-model"));

        // Some(non-empty) sets the key; a None sibling leaves model unchanged.
        s.apply_update(AiModelSettingsUpdate {
            api_key: Some("sk-new".into()),
            ..Default::default()
        });
        assert_eq!(s.api_key.as_deref(), Some("sk-new"));
        assert_eq!(s.model.as_deref(), Some("example-model"));

        // Some("") clears the key.
        s.apply_update(AiModelSettingsUpdate {
            api_key: Some(String::new()),
            ..Default::default()
        });
        assert!(s.api_key.is_none());
    }

    /// Non-secret fields update with `Some`, stay with `None`.
    #[test]
    fn update_non_secret_fields() {
        let mut s = AiModelSettings::default();
        s.apply_update(AiModelSettingsUpdate {
            provider: Some("openai-compatible".into()),
            max_context_bytes: Some(65_536),
            response_format: Some(ResponseFormatMode::JsonSchema),
            ..Default::default()
        });
        assert_eq!(s.provider.as_deref(), Some("openai-compatible"));
        assert_eq!(s.max_context_bytes, Some(65_536));
        assert_eq!(s.response_format, ResponseFormatMode::JsonSchema);

        // A None response_format leaves the mode unchanged.
        s.apply_update(AiModelSettingsUpdate::default());
        assert_eq!(s.response_format, ResponseFormatMode::JsonSchema);
    }

    /// `is_configured` requires model, base URL, and a non-empty key together;
    /// any one missing or blank reports unconfigured.
    #[test]
    fn is_configured_requires_model_base_url_and_key() {
        assert!(configured().is_configured());

        // Default (all None) is unconfigured.
        assert!(!AiModelSettings::default().is_configured());

        // Each field missing in turn fails the check.
        let mut s = configured();
        s.model = None;
        assert!(!s.is_configured());

        let mut s = configured();
        s.base_url = Some(String::new());
        assert!(!s.is_configured());

        let mut s = configured();
        s.api_key = None;
        assert!(!s.is_configured());
    }

    /// The default response format is `json_object` (back-compat with configs
    /// written before the field existed, via `#[serde(default)]`).
    #[test]
    fn response_format_defaults_to_json_object() {
        assert_eq!(
            AiModelSettings::default().response_format,
            ResponseFormatMode::JsonObject
        );
        // A TOML/JSON config without the field deserializes to the default.
        let s: AiModelSettings = serde_json::from_str("{}").expect("empty config");
        assert_eq!(s.response_format, ResponseFormatMode::JsonObject);
    }

    /// Default execution mode is `suggest_only`, and a config written before the
    /// field existed deserializes to it (back-compat via the struct's
    /// `#[serde(default)]`).
    #[test]
    fn execution_mode_defaults_to_suggest_only() {
        assert_eq!(
            AiModelSettings::default().execution_mode,
            ExecutionMode::SuggestOnly
        );
        let s: AiModelSettings = serde_json::from_str("{}").expect("empty config");
        assert_eq!(s.execution_mode, ExecutionMode::SuggestOnly);
    }

    /// Update accepts the three M2-selectable modes and ignores the
    /// not-yet-selectable ones, so the persisted value never leaves the usable
    /// set.
    #[test]
    fn update_execution_mode_rejects_non_selectable() {
        let mut s = configured();
        assert_eq!(s.execution_mode, ExecutionMode::SuggestOnly);

        for mode in [
            ExecutionMode::ReadOnly,
            ExecutionMode::ConfirmEachAction,
            ExecutionMode::SuggestOnly,
        ] {
            s.apply_update(AiModelSettingsUpdate {
                execution_mode: Some(mode),
                ..Default::default()
            });
            assert_eq!(s.execution_mode, mode);
        }

        // Set a known good value, then a not-selectable mode is ignored.
        s.apply_update(AiModelSettingsUpdate {
            execution_mode: Some(ExecutionMode::ConfirmEachAction),
            ..Default::default()
        });
        for mode in [ExecutionMode::SessionApproved, ExecutionMode::Automated] {
            s.apply_update(AiModelSettingsUpdate {
                execution_mode: Some(mode),
                ..Default::default()
            });
            assert_eq!(
                s.execution_mode,
                ExecutionMode::ConfirmEachAction,
                "not-selectable mode {mode:?} must not be persisted"
            );
        }

        // None leaves the stored mode unchanged.
        s.apply_update(AiModelSettingsUpdate::default());
        assert_eq!(s.execution_mode, ExecutionMode::ConfirmEachAction);
    }

    /// The public view carries the execution mode (it is not a secret).
    #[test]
    fn public_view_reports_execution_mode() {
        let mut s = configured();
        s.apply_update(AiModelSettingsUpdate {
            execution_mode: Some(ExecutionMode::ConfirmEachAction),
            ..Default::default()
        });
        assert_eq!(
            s.public_view().execution_mode,
            ExecutionMode::ConfirmEachAction
        );
    }

    /// Default gateway mode is `direct`, and a config written before the field
    /// existed deserializes to it (back-compat via the struct's `#[serde(default)]`).
    #[test]
    fn gateway_mode_defaults_to_direct() {
        assert_eq!(AiModelSettings::default().gateway_mode, GatewayMode::Direct);
        let s: AiModelSettings = serde_json::from_str("{}").expect("empty config");
        assert_eq!(s.gateway_mode, GatewayMode::Direct);
    }

    /// Update sets the gateway mode with `Some` and leaves it with `None`, and
    /// the public view carries it (it is not a secret).
    #[test]
    fn update_and_public_view_gateway_mode() {
        let mut s = configured();
        assert_eq!(s.gateway_mode, GatewayMode::Direct);

        s.apply_update(AiModelSettingsUpdate {
            gateway_mode: Some(GatewayMode::ManagerProxy),
            ..Default::default()
        });
        assert_eq!(s.gateway_mode, GatewayMode::ManagerProxy);
        assert_eq!(s.public_view().gateway_mode, GatewayMode::ManagerProxy);

        // None leaves the stored mode unchanged.
        s.apply_update(AiModelSettingsUpdate::default());
        assert_eq!(s.gateway_mode, GatewayMode::ManagerProxy);
    }

    /// Adding `gateway_mode` must not widen the secret boundary: the public view
    /// of a manager-proxy config still never carries the key.
    #[test]
    fn public_view_with_gateway_mode_still_masks_key() {
        let mut s = configured();
        s.gateway_mode = GatewayMode::ManagerProxy;
        let json = serde_json::to_string(&s.public_view()).expect("serialize public");
        assert!(
            json.contains("manager_proxy"),
            "gateway_mode missing: {json}"
        );
        assert!(
            !json.contains("sk-secret-value"),
            "public view leaked key: {json}"
        );
        assert!(
            !json.contains("api_key\""),
            "public view carries api_key: {json}"
        );
    }

    /// Regression for the secret boundary: the legacy `/settings` payload is
    /// `SystemSettings`, a sibling of `ai_model` on `Settings`. Even with a
    /// model key configured, the `SystemSettings` JSON can never carry it.
    #[test]
    fn legacy_system_settings_payload_excludes_model_key() {
        let mut settings = crate::model::settings::Settings::default();
        settings.ai_model.api_key = Some("sk-must-not-appear".into());
        let payload = serde_json::to_string(&settings.system).expect("serialize SystemSettings");
        assert!(
            !payload.contains("sk-must-not-appear"),
            "legacy /settings payload leaked the model key: {payload}"
        );
        assert!(
            !payload.contains("ai_model"),
            "SystemSettings must not embed ai_model: {payload}"
        );
    }

    /// `Debug` must redact the key — covers the log path that does not go
    /// through serialization.
    #[test]
    fn debug_redacts_the_key() {
        let rendered = format!("{:?}", configured());
        assert!(
            !rendered.contains("sk-secret-value"),
            "Debug leaked the key: {rendered}"
        );
        assert!(
            rendered.contains("***"),
            "Debug should mark the key present: {rendered}"
        );

        let update = AiModelSettingsUpdate {
            api_key: Some("sk-secret-value".into()),
            ..Default::default()
        };
        let rendered = format!("{update:?}");
        assert!(
            !rendered.contains("sk-secret-value"),
            "update Debug leaked the key: {rendered}"
        );
    }
}
