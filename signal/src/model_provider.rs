//! Model-provider configuration for the OSS signal central brain, with a hard
//! secret boundary mirroring the edge's `ai_model` settings.
//!
//! The signal server is the central brain in the OSS "thin edge + central brain"
//! split: it owns the model credentials and dials the provider, while edges only
//! offer device-capability interfaces. Because the portable signal server is
//! single-node and single-account, there is exactly one provider config
//! (persisted as the singleton row in [`crate::entity::model_provider`]).
//!
//! The secret boundary has three faces (matching the edge `ai_model` design):
//! - [`ModelProviderConfig`] — the loaded form. `api_key` is plaintext in the
//!   local sqlite row but its [`std::fmt::Debug`] is redacted.
//! - [`ModelProviderPublic`] — what `GET` returns. It reports only whether a key
//!   is configured (`api_key_set`), never the key itself.
//! - [`ModelProviderUpdate`] — what `POST` accepts. `api_key` is write-only with
//!   explicit leave / clear / set semantics.

use std::fmt;

use desk_agent_protocol::ExecutionMode;
use sea_orm::ActiveValue::Set;
use sea_orm::{DatabaseConnection, DbErr, EntityTrait};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::entity::model_provider::{self, SINGLETON_ID};

/// Whether an [`ExecutionMode`] is one the confirm-execute flow supports.
/// `SessionApproved` / `Automated` are frozen in the protocol enum but not
/// selectable yet; persisting them is rejected so the stored grant stays in the
/// usable set. Mirrors the edge `ai_model` guard.
fn is_selectable(mode: ExecutionMode) -> bool {
    matches!(
        mode,
        ExecutionMode::SuggestOnly | ExecutionMode::ReadOnly | ExecutionMode::ConfirmEachAction
    )
}

/// How the model gateway is asked to constrain its output format.
///
/// The diagnosis parser degrades gracefully regardless of this setting, so it is
/// purely an enforcement hint to the gateway. This is a signal-local copy of the
/// edge's enum (the two crates keep separate implementations of the same shape).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResponseFormatMode {
    /// No `response_format` is sent; the model may return prose.
    None,
    /// Request syntactically valid JSON (`{"type":"json_object"}`). The default.
    #[default]
    JsonObject,
    /// Request the diagnosis JSON schema (`{"type":"json_schema",...}`).
    JsonSchema,
}

/// Encode a `serde(rename_all = "snake_case")` enum to its bare wire string
/// (e.g. `ExecutionMode::SuggestOnly` -> `"suggest_only"`), without the quotes
/// `serde_json::to_string` would add.
fn enum_to_wire<T: Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// Decode a bare wire string back to its enum, falling back to the default when
/// the stored string is unrecognized (forward/backward tolerant).
fn enum_from_wire<T: serde::de::DeserializeOwned + Default>(raw: &str) -> T {
    serde_json::from_value(serde_json::Value::String(raw.to_owned())).unwrap_or_default()
}

/// Loaded model-provider configuration (the central brain's credentials + policy
/// defaults).
///
/// `Debug` is implemented by hand so `api_key` is never rendered.
#[derive(Clone, Default)]
pub struct ModelProviderConfig {
    /// Provider identifier, e.g. `"openai-compatible"`.
    pub provider: Option<String>,
    /// Model name passed to the provider.
    pub model: Option<String>,
    /// Base URL of the (OpenAI-compatible) chat completions endpoint.
    pub base_url: Option<String>,
    /// Server-side secret. Never serialized into a public view; its `Debug` is
    /// redacted.
    pub api_key: Option<String>,
    /// Upper bound on the structured evidence sent to the model, in bytes.
    pub max_context_bytes: Option<u64>,
    /// How the gateway is asked to constrain output format.
    pub response_format: ResponseFormatMode,
    /// The execution-mode grant the central brain stamps into the authorization
    /// it issues to edges. Edges still apply their own local ceiling on top, so
    /// this is the granted breadth, not the final one.
    pub execution_mode: ExecutionMode,
}

impl ModelProviderConfig {
    /// Whether a non-empty API key is configured.
    pub fn api_key_set(&self) -> bool {
        self.api_key.as_deref().is_some_and(|k| !k.is_empty())
    }

    /// Whether the provider has the minimum fields needed to attempt a call:
    /// `model`, `base_url`, and `api_key` all present and non-empty.
    pub fn is_configured(&self) -> bool {
        let nonempty = |o: &Option<String>| o.as_deref().is_some_and(|v| !v.is_empty());
        nonempty(&self.model) && nonempty(&self.base_url) && self.api_key_set()
    }

    /// Project the masked public view returned by the query endpoint.
    pub fn public_view(&self) -> ModelProviderPublic {
        ModelProviderPublic {
            provider: self.provider.clone(),
            model: self.model.clone(),
            base_url: self.base_url.clone(),
            max_context_bytes: self.max_context_bytes,
            response_format: self.response_format,
            execution_mode: self.execution_mode,
            api_key_set: self.api_key_set(),
        }
    }

    /// Apply an update in place. Non-secret fields use `None` = leave unchanged;
    /// `api_key` additionally treats `Some("")` as clear and `Some(non-empty)`
    /// as set. A not-yet-selectable execution mode is ignored.
    pub fn apply_update(&mut self, update: ModelProviderUpdate) {
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
        if let Some(execution_mode) = update.execution_mode
            && is_selectable(execution_mode)
        {
            self.execution_mode = execution_mode;
        }
        match update.api_key {
            None => {}                                          // leave unchanged
            Some(key) if key.is_empty() => self.api_key = None, // clear
            Some(key) => self.api_key = Some(key),              // set
        }
    }

    fn from_entity(row: model_provider::Model) -> Self {
        Self {
            provider: row.provider,
            model: row.model,
            base_url: row.base_url,
            api_key: row.api_key,
            max_context_bytes: row.max_context_bytes.map(|v| v.max(0) as u64),
            response_format: enum_from_wire(&row.response_format),
            execution_mode: enum_from_wire(&row.execution_mode),
        }
    }

    fn into_active_model(self) -> model_provider::ActiveModel {
        model_provider::ActiveModel {
            id: Set(SINGLETON_ID),
            provider: Set(self.provider),
            model: Set(self.model),
            base_url: Set(self.base_url),
            api_key: Set(self.api_key),
            max_context_bytes: Set(self
                .max_context_bytes
                .map(|v| v.min(i64::MAX as u64) as i64)),
            response_format: Set(enum_to_wire(&self.response_format)),
            execution_mode: Set(enum_to_wire(&self.execution_mode)),
            updated_at: Set(chrono::Utc::now()),
        }
    }
}

impl fmt::Debug for ModelProviderConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModelProviderConfig")
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("base_url", &self.base_url)
            // Redact: report presence, never the value.
            .field("api_key", &self.api_key.as_ref().map(|_| "***"))
            .field("max_context_bytes", &self.max_context_bytes)
            .field("response_format", &self.response_format)
            .field("execution_mode", &self.execution_mode)
            .finish()
    }
}

/// Masked public view returned by the provider-config query endpoint. Carries no
/// secret: only whether a key is configured (`api_key_set`).
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema, Default)]
pub struct ModelProviderPublic {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub max_context_bytes: Option<u64>,
    pub response_format: ResponseFormatMode,
    pub execution_mode: ExecutionMode,
    /// Whether a non-empty API key is configured. The key itself is never
    /// returned.
    pub api_key_set: bool,
}

/// Update body for the provider-config update endpoint.
///
/// Every field is optional: `None` leaves the stored value unchanged. `api_key`
/// is write-only with three-way semantics (see [`ModelProviderConfig::apply_update`]).
#[derive(Clone, Serialize, Deserialize, ToSchema, Default)]
pub struct ModelProviderUpdate {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub max_context_bytes: Option<u64>,
    /// `None` leaves the stored format unchanged.
    pub response_format: Option<ResponseFormatMode>,
    /// `None` leaves the stored grant unchanged. A not-yet-selectable mode
    /// (`session_approved` / `automated`) is ignored.
    pub execution_mode: Option<ExecutionMode>,
    /// Write-only. `None` = leave unchanged; `Some("")` = clear; `Some(x)` = set.
    pub api_key: Option<String>,
}

impl fmt::Debug for ModelProviderUpdate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModelProviderUpdate")
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("base_url", &self.base_url)
            .field("max_context_bytes", &self.max_context_bytes)
            .field("response_format", &self.response_format)
            .field("execution_mode", &self.execution_mode)
            .field("api_key", &self.api_key.as_ref().map(|_| "***"))
            .finish()
    }
}

/// Load the singleton provider config, returning the default (all-unset) config
/// when no row has been written yet.
pub async fn load(db: &DatabaseConnection) -> Result<ModelProviderConfig, DbErr> {
    let row = model_provider::Entity::find_by_id(SINGLETON_ID)
        .one(db)
        .await?;
    Ok(row
        .map(ModelProviderConfig::from_entity)
        .unwrap_or_default())
}

/// Persist the singleton provider config (insert-or-replace on the fixed PK).
pub async fn save(db: &DatabaseConnection, config: ModelProviderConfig) -> Result<(), DbErr> {
    use sea_orm::sea_query::OnConflict;
    let active = config.into_active_model();
    model_provider::Entity::insert(active)
        .on_conflict(
            OnConflict::column(model_provider::Column::Id)
                .update_columns([
                    model_provider::Column::Provider,
                    model_provider::Column::Model,
                    model_provider::Column::BaseUrl,
                    model_provider::Column::ApiKey,
                    model_provider::Column::MaxContextBytes,
                    model_provider::Column::ResponseFormat,
                    model_provider::Column::ExecutionMode,
                    model_provider::Column::UpdatedAt,
                ])
                .to_owned(),
        )
        .exec(db)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::{ConnectionTrait, Database, Schema};

    async fn memory_db() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        let schema = Schema::new(db.get_database_backend());
        let stmt = schema.create_table_from_entity(model_provider::Entity);
        db.execute(&stmt).await.unwrap();
        db
    }

    fn configured() -> ModelProviderConfig {
        ModelProviderConfig {
            provider: Some("openai-compatible".into()),
            model: Some("example-model".into()),
            base_url: Some("https://api.example/v1".into()),
            api_key: Some("sk-secret-value".into()),
            max_context_bytes: Some(131_072),
            response_format: ResponseFormatMode::JsonObject,
            execution_mode: ExecutionMode::SuggestOnly,
        }
    }

    #[test]
    fn public_view_masks_the_key() {
        let public = configured().public_view();
        assert!(public.api_key_set);
        let json = serde_json::to_string(&public).expect("serialize public");
        assert!(!json.contains("sk-secret-value"), "leaked key: {json}");
        assert!(!json.contains("api_key\""), "carries api_key: {json}");
    }

    #[test]
    fn api_key_set_treats_empty_as_unset() {
        let mut s = ModelProviderConfig::default();
        assert!(!s.api_key_set());
        s.api_key = Some(String::new());
        assert!(!s.api_key_set());
        s.api_key = Some("k".into());
        assert!(s.api_key_set());
    }

    #[test]
    fn update_api_key_three_way_semantics() {
        let mut s = configured();
        // None leaves everything unchanged.
        s.apply_update(ModelProviderUpdate::default());
        assert_eq!(s.api_key.as_deref(), Some("sk-secret-value"));
        assert_eq!(s.model.as_deref(), Some("example-model"));
        // Some(non-empty) sets the key.
        s.apply_update(ModelProviderUpdate {
            api_key: Some("sk-new".into()),
            ..Default::default()
        });
        assert_eq!(s.api_key.as_deref(), Some("sk-new"));
        // Some("") clears it.
        s.apply_update(ModelProviderUpdate {
            api_key: Some(String::new()),
            ..Default::default()
        });
        assert!(s.api_key.is_none());
    }

    #[test]
    fn is_configured_requires_model_base_url_and_key() {
        assert!(configured().is_configured());
        assert!(!ModelProviderConfig::default().is_configured());
        let mut s = configured();
        s.model = None;
        assert!(!s.is_configured());
        let mut s = configured();
        s.api_key = None;
        assert!(!s.is_configured());
    }

    #[test]
    fn update_execution_mode_rejects_non_selectable() {
        let mut s = configured();
        for mode in [
            ExecutionMode::ReadOnly,
            ExecutionMode::ConfirmEachAction,
            ExecutionMode::SuggestOnly,
        ] {
            s.apply_update(ModelProviderUpdate {
                execution_mode: Some(mode),
                ..Default::default()
            });
            assert_eq!(s.execution_mode, mode);
        }
        s.apply_update(ModelProviderUpdate {
            execution_mode: Some(ExecutionMode::ConfirmEachAction),
            ..Default::default()
        });
        for mode in [ExecutionMode::SessionApproved, ExecutionMode::Automated] {
            s.apply_update(ModelProviderUpdate {
                execution_mode: Some(mode),
                ..Default::default()
            });
            assert_eq!(
                s.execution_mode,
                ExecutionMode::ConfirmEachAction,
                "not-selectable mode {mode:?} must not be persisted"
            );
        }
    }

    #[test]
    fn debug_redacts_the_key() {
        let rendered = format!("{:?}", configured());
        assert!(!rendered.contains("sk-secret-value"), "leaked: {rendered}");
        assert!(rendered.contains("***"), "should mark present: {rendered}");
    }

    #[tokio::test]
    async fn load_default_when_absent() {
        let db = memory_db().await;
        let cfg = load(&db).await.unwrap();
        assert!(!cfg.is_configured());
        assert_eq!(cfg.execution_mode, ExecutionMode::SuggestOnly);
    }

    #[tokio::test]
    async fn save_then_load_round_trips_including_enums() {
        let db = memory_db().await;
        let mut cfg = configured();
        cfg.response_format = ResponseFormatMode::JsonSchema;
        cfg.execution_mode = ExecutionMode::ConfirmEachAction;
        save(&db, cfg).await.unwrap();

        let loaded = load(&db).await.unwrap();
        assert_eq!(loaded.model.as_deref(), Some("example-model"));
        assert_eq!(loaded.api_key.as_deref(), Some("sk-secret-value"));
        assert_eq!(loaded.max_context_bytes, Some(131_072));
        assert_eq!(loaded.response_format, ResponseFormatMode::JsonSchema);
        assert_eq!(loaded.execution_mode, ExecutionMode::ConfirmEachAction);
    }

    #[tokio::test]
    async fn save_is_idempotent_on_singleton_row() {
        let db = memory_db().await;
        save(&db, configured()).await.unwrap();
        let mut second = configured();
        second.model = Some("other-model".into());
        save(&db, second).await.unwrap();

        // Still a single row, holding the latest write.
        let rows = model_provider::Entity::find().all(&db).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].model.as_deref(), Some("other-model"));
    }
}
