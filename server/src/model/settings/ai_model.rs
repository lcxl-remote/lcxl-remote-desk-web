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

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

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
    /// Whether screenshots may be sent to the model. Default `false`.
    pub allow_screen: bool,
    /// Whether logs may be sent to the model. Default `false`.
    pub allow_logs: bool,
    /// Upper bound on the structured evidence sent to the model, in bytes.
    pub max_context_bytes: Option<u64>,
    /// How the gateway is asked to constrain output format. Default
    /// `json_object`.
    pub response_format: ResponseFormatMode,
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
            allow_screen: self.allow_screen,
            allow_logs: self.allow_logs,
            max_context_bytes: self.max_context_bytes,
            response_format: self.response_format,
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
        if let Some(allow_screen) = update.allow_screen {
            self.allow_screen = allow_screen;
        }
        if let Some(allow_logs) = update.allow_logs {
            self.allow_logs = allow_logs;
        }
        if let Some(max_context_bytes) = update.max_context_bytes {
            self.max_context_bytes = Some(max_context_bytes);
        }
        if let Some(response_format) = update.response_format {
            self.response_format = response_format;
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
            .field("allow_screen", &self.allow_screen)
            .field("allow_logs", &self.allow_logs)
            .field("max_context_bytes", &self.max_context_bytes)
            .field("response_format", &self.response_format)
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
    pub allow_screen: bool,
    pub allow_logs: bool,
    pub max_context_bytes: Option<u64>,
    pub response_format: ResponseFormatMode,
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
    pub allow_screen: Option<bool>,
    pub allow_logs: Option<bool>,
    pub max_context_bytes: Option<u64>,
    /// `None` leaves the stored mode unchanged.
    pub response_format: Option<ResponseFormatMode>,
    /// Write-only. `None` = leave unchanged; `Some("")` = clear; `Some(x)` = set.
    pub api_key: Option<String>,
}

impl fmt::Debug for AiModelSettingsUpdate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AiModelSettingsUpdate")
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("base_url", &self.base_url)
            .field("allow_screen", &self.allow_screen)
            .field("allow_logs", &self.allow_logs)
            .field("max_context_bytes", &self.max_context_bytes)
            .field("response_format", &self.response_format)
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
            allow_screen: false,
            allow_logs: false,
            max_context_bytes: Some(131_072),
            response_format: ResponseFormatMode::JsonObject,
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
            allow_logs: Some(true),
            max_context_bytes: Some(65_536),
            response_format: Some(ResponseFormatMode::JsonSchema),
            ..Default::default()
        });
        assert_eq!(s.provider.as_deref(), Some("openai-compatible"));
        assert!(s.allow_logs);
        assert!(!s.allow_screen); // untouched
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
