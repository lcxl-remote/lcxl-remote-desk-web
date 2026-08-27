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
use desk_agent_protocol::data_lineage::DestinationIdentity;
use desk_diagnose_core::model_profile::{
    MODEL_PROFILE_SCHEMA_VERSION, ModelRequestProfile, OutputLimitField, ProfileError, WireProtocol,
};
use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::{Expr, OnConflict};
use sea_orm::{
    ColumnTrait, DatabaseConnection, DbErr, EntityTrait, QueryFilter, TransactionTrait,
    TryInsertResult,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::entity::model_provider::SINGLETON_ID;
use crate::entity::{model_probe_observation, model_provider};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ModelProbeObservation {
    pub connection_revision: i64,
    pub profile_revision: i64,
    pub tested_at: chrono::DateTime<chrono::Utc>,
    pub reasoning_observed: bool,
    pub reasoning_tokens: Option<i64>,
    pub stop_reason: Option<String>,
    #[schema(value_type = Object)]
    pub validated_capabilities: serde_json::Value,
    /// Whether the observation still describes the currently saved connection
    /// and profile revisions. Stale observations remain visible as history but
    /// must never be presented as current validation.
    pub current: bool,
}

pub const MAX_STEPS_MIN: u32 = desk_diagnose_core::MIN_STEPS_PER_TURN;
pub const MAX_STEPS_MAX: u32 = desk_diagnose_core::MAX_STEPS_PER_TURN_LIMIT;
pub const MAX_STEPS_DEFAULT: u32 = desk_diagnose_core::MAX_STEPS_PER_TURN;
pub const MAX_SAME_TOOL_CALLS_MIN: u32 = desk_diagnose_core::MIN_SAME_TOOL_PER_TURN;
pub const MAX_SAME_TOOL_CALLS_MAX: u32 = desk_diagnose_core::MAX_SAME_TOOL_PER_TURN_LIMIT;
pub const MAX_SAME_TOOL_CALLS_DEFAULT: u32 = desk_diagnose_core::MAX_SAME_TOOL_PER_TURN;
pub const EXEC_APPROVAL_TIMEOUT_MIN_SECS: u32 = 30;
pub const EXEC_APPROVAL_TIMEOUT_MAX_SECS: u32 = 1800;
pub const EXEC_APPROVAL_TIMEOUT_DEFAULT_SECS: u32 = 120;

pub fn step_budget_covers_same_tool_limit(max_steps: u32, same_tool_limit: u32) -> bool {
    max_steps >= same_tool_limit
}

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
#[derive(Clone)]
pub struct ModelProviderConfig {
    /// Provider wire contract. `None` is allowed only for an unconfigured row.
    pub wire_protocol: Option<WireProtocol>,
    /// Model name passed to the provider.
    pub model: Option<String>,
    /// Whether the configured model accepts image content in user messages.
    pub supports_image_input: bool,
    /// Base URL of the (OpenAI-compatible) chat completions endpoint.
    pub base_url: Option<String>,
    /// Server-side secret. Never serialized into a public view; its `Debug` is
    /// redacted.
    pub api_key: Option<String>,
    pub profile_schema_version: u16,
    pub request_options: serde_json::Value,
    pub output_limit_field: OutputLimitField,
    pub probe_max_output_tokens: i64,
    pub runtime_max_output_tokens: i64,
    /// Explicit local model-history budget. It is not inferred from the model.
    pub max_context_bytes: Option<i64>,
    pub connection_revision: i64,
    pub profile_revision: i64,
    pub probe_observation: Option<ModelProbeObservation>,
    /// How the gateway is asked to constrain output format.
    pub response_format: ResponseFormatMode,
    /// The execution-mode grant the central brain stamps into the authorization
    /// it issues to edges. Edges still apply their own local ceiling on top, so
    /// this is the granted breadth, not the final one.
    pub execution_mode: ExecutionMode,
    /// Per-turn circuit-breaker cap for calls to one tool name. This is a
    /// central agent-runtime limit, not an edge command-concurrency limit.
    pub max_same_tool_calls_per_turn: u32,
    /// Per-turn model reasoning-round budget. One round may contain multiple
    /// tool calls and the final answer also consumes a round.
    pub max_steps_per_turn: u32,
    /// How long a newly created owner-confirmed command approval remains open.
    pub exec_approval_timeout_secs: u32,
}

impl Default for ModelProviderConfig {
    fn default() -> Self {
        Self {
            wire_protocol: None,
            model: None,
            supports_image_input: false,
            base_url: None,
            api_key: None,
            profile_schema_version: MODEL_PROFILE_SCHEMA_VERSION,
            request_options: serde_json::json!({}),
            output_limit_field: OutputLimitField::MaxTokens,
            probe_max_output_tokens: 512,
            runtime_max_output_tokens: 4096,
            max_context_bytes: None,
            connection_revision: 1,
            profile_revision: 1,
            probe_observation: None,
            response_format: ResponseFormatMode::default(),
            // Grant confirmed execution centrally by default. The target edge's
            // local AI policy remains an independent ceiling, and every command
            // still requires the operator's one-shot approval.
            execution_mode: ExecutionMode::ConfirmEachAction,
            max_same_tool_calls_per_turn: MAX_SAME_TOOL_CALLS_DEFAULT,
            max_steps_per_turn: MAX_STEPS_DEFAULT,
            exec_approval_timeout_secs: EXEC_APPROVAL_TIMEOUT_DEFAULT_SECS,
        }
    }
}

impl ModelProviderConfig {
    /// Resolve the exact model egress destination from the existing OSS AI
    /// gateway row. This projection carries stable identity/revisions only and
    /// can never copy the base URL or credential into a DataEnvelope.
    pub fn destination_identity(&self) -> Result<DestinationIdentity, ModelDestinationError> {
        if !self.is_configured() {
            return Err(ModelDestinationError::NotConfigured);
        }
        let model_id = self
            .model
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or(ModelDestinationError::NotConfigured)?;
        let connection_revision = u64::try_from(self.connection_revision)
            .map_err(|_| ModelDestinationError::InvalidRevision)?;
        let profile_revision = self.profile_revision;
        if connection_revision == 0 || profile_revision < 1 {
            return Err(ModelDestinationError::InvalidRevision);
        }
        Ok(DestinationIdentity::Model {
            connection_id: format!("oss-ai-gateway:{SINGLETON_ID}"),
            connection_revision,
            model_id: model_id.to_string(),
            profile_revision,
        })
    }

    pub fn request_profile(&self) -> Result<ModelRequestProfile, ProfileError> {
        let max_context_bytes = self
            .max_context_bytes
            .ok_or(ProfileError::InvalidMaxContextBytes(0))?;
        let profile = ModelRequestProfile {
            profile_schema_version: self.profile_schema_version,
            request_options: self.request_options.clone(),
            output_limit_field: self.output_limit_field,
            probe_max_output_tokens: self.probe_max_output_tokens,
            runtime_max_output_tokens: self.runtime_max_output_tokens,
            max_context_bytes,
            profile_revision: self.profile_revision,
        };
        let protocol = self
            .wire_protocol
            .ok_or_else(|| ProfileError::UnknownWireProtocol(String::new()))?;
        profile.validate(protocol)?;
        Ok(profile)
    }

    /// Whether a non-empty API key is configured.
    pub fn api_key_set(&self) -> bool {
        self.api_key.as_deref().is_some_and(|k| !k.is_empty())
    }

    /// Whether the provider has the minimum fields needed to attempt a call:
    /// `model`, `base_url`, and `api_key` all present and non-empty.
    pub fn is_configured(&self) -> bool {
        let nonempty = |o: &Option<String>| o.as_deref().is_some_and(|v| !v.is_empty());
        self.wire_protocol.is_some()
            && nonempty(&self.model)
            && nonempty(&self.base_url)
            && self.api_key_set()
            && self.request_profile().is_ok()
    }

    /// Project the masked public view returned by the query endpoint.
    pub fn public_view(&self) -> ModelProviderPublic {
        ModelProviderPublic {
            wire_protocol: self.wire_protocol,
            model: self.model.clone(),
            supports_image_input: self.supports_image_input,
            base_url: self.base_url.clone(),
            max_context_bytes: self.max_context_bytes,
            profile_schema_version: self.profile_schema_version,
            request_options: self.request_options.clone(),
            output_limit_field: self.output_limit_field,
            probe_max_output_tokens: self.probe_max_output_tokens,
            runtime_max_output_tokens: self.runtime_max_output_tokens,
            connection_revision: self.connection_revision,
            profile_revision: self.profile_revision,
            probe_observation: self.probe_observation.clone(),
            response_format: self.response_format,
            execution_mode: self.execution_mode,
            max_same_tool_calls_per_turn: self.max_same_tool_calls_per_turn,
            max_steps_per_turn: self.max_steps_per_turn,
            exec_approval_timeout_secs: self.exec_approval_timeout_secs,
            api_key_set: self.api_key_set(),
        }
    }

    /// Apply an update in place. Non-secret fields use `None` = leave unchanged;
    /// `api_key` additionally treats `Some("")` as clear and `Some(non-empty)`
    /// as set. A not-yet-selectable execution mode is ignored.
    pub fn apply_update(&mut self, update: ModelProviderUpdate) {
        let connection_changed = update
            .wire_protocol
            .is_some_and(|value| Some(value) != self.wire_protocol)
            || update
                .base_url
                .as_ref()
                .is_some_and(|value| Some(value) != self.base_url.as_ref())
            || update.api_key.as_ref().is_some_and(|value| {
                let next = (!value.is_empty()).then_some(value);
                next != self.api_key.as_ref()
            });
        let profile_changed = update
            .model
            .as_ref()
            .is_some_and(|value| Some(value) != self.model.as_ref())
            || update
                .supports_image_input
                .is_some_and(|value| value != self.supports_image_input)
            || update
                .request_options
                .as_ref()
                .is_some_and(|value| value != &self.request_options)
            || update
                .output_limit_field
                .is_some_and(|value| value != self.output_limit_field)
            || update
                .probe_max_output_tokens
                .is_some_and(|value| value != self.probe_max_output_tokens)
            || update
                .runtime_max_output_tokens
                .is_some_and(|value| value != self.runtime_max_output_tokens)
            || update
                .max_context_bytes
                .is_some_and(|value| Some(value) != self.max_context_bytes);
        if let Some(wire_protocol) = update.wire_protocol {
            self.wire_protocol = Some(wire_protocol);
        }
        if let Some(model) = update.model {
            self.model = Some(model);
        }
        if let Some(supports_image_input) = update.supports_image_input {
            self.supports_image_input = supports_image_input;
        }
        if let Some(base_url) = update.base_url {
            self.base_url = Some(base_url);
        }
        if let Some(max_context_bytes) = update.max_context_bytes {
            self.max_context_bytes = Some(max_context_bytes);
        }
        if let Some(request_options) = update.request_options {
            self.request_options = request_options;
        }
        if let Some(output_limit_field) = update.output_limit_field {
            self.output_limit_field = output_limit_field;
        }
        if let Some(limit) = update.probe_max_output_tokens {
            self.probe_max_output_tokens = limit;
        }
        if let Some(limit) = update.runtime_max_output_tokens {
            self.runtime_max_output_tokens = limit;
        }
        if let Some(response_format) = update.response_format {
            self.response_format = response_format;
        }
        if let Some(execution_mode) = update.execution_mode
            && is_selectable(execution_mode)
        {
            self.execution_mode = execution_mode;
        }
        if let Some(limit) = update.max_same_tool_calls_per_turn {
            self.max_same_tool_calls_per_turn =
                limit.clamp(MAX_SAME_TOOL_CALLS_MIN, MAX_SAME_TOOL_CALLS_MAX);
        }
        if let Some(limit) = update.max_steps_per_turn {
            self.max_steps_per_turn = limit.clamp(MAX_STEPS_MIN, MAX_STEPS_MAX);
        }
        if let Some(timeout) = update.exec_approval_timeout_secs {
            self.exec_approval_timeout_secs = timeout.clamp(
                EXEC_APPROVAL_TIMEOUT_MIN_SECS,
                EXEC_APPROVAL_TIMEOUT_MAX_SECS,
            );
        }
        // Keep the cross-field invariant even for non-HTTP callers or legacy
        // stored values. The API rejects this shape; the domain layer repairs it.
        self.max_steps_per_turn = self
            .max_steps_per_turn
            .max(self.max_same_tool_calls_per_turn);
        match update.api_key {
            None => {}                                          // leave unchanged
            Some(key) if key.is_empty() => self.api_key = None, // clear
            Some(key) => self.api_key = Some(key),              // set
        }
        if connection_changed {
            self.connection_revision = self.connection_revision.saturating_add(1).max(1);
            self.probe_observation = None;
        }
        if profile_changed {
            self.profile_revision = self.profile_revision.saturating_add(1).max(1);
            self.probe_observation = None;
        }
    }

    fn from_entity(row: model_provider::Model) -> Self {
        let exec_approval_timeout_secs = u32::try_from(row.exec_approval_timeout_secs)
            .ok()
            .filter(|value| {
                (EXEC_APPROVAL_TIMEOUT_MIN_SECS..=EXEC_APPROVAL_TIMEOUT_MAX_SECS).contains(value)
            })
            .unwrap_or_else(|| {
                log::warn!(
                    "stored exec approval timeout is invalid; defaulting to \
                     {EXEC_APPROVAL_TIMEOUT_DEFAULT_SECS} seconds"
                );
                EXEC_APPROVAL_TIMEOUT_DEFAULT_SECS
            });
        Self {
            wire_protocol: row
                .wire_protocol
                .as_deref()
                .map(WireProtocol::parse)
                .transpose()
                .unwrap_or(None),
            model: row.model,
            supports_image_input: row.supports_image_input,
            base_url: row.base_url,
            api_key: row.api_key,
            profile_schema_version: u16::try_from(row.profile_schema_version).unwrap_or(0),
            request_options: serde_json::from_str(&row.request_options)
                .unwrap_or(serde_json::Value::Null),
            output_limit_field: OutputLimitField::parse(&row.output_limit_field)
                .unwrap_or(OutputLimitField::MaxOutputTokens),
            probe_max_output_tokens: row.probe_max_output_tokens,
            runtime_max_output_tokens: row.runtime_max_output_tokens,
            max_context_bytes: Some(row.max_context_bytes),
            connection_revision: row.connection_revision,
            profile_revision: row.profile_revision,
            probe_observation: None,
            response_format: enum_from_wire(&row.response_format),
            execution_mode: enum_from_wire(&row.execution_mode),
            max_same_tool_calls_per_turn: (row.max_same_tool_calls_per_turn.max(0) as u32)
                .clamp(MAX_SAME_TOOL_CALLS_MIN, MAX_SAME_TOOL_CALLS_MAX),
            max_steps_per_turn: (row.max_steps_per_turn.max(0) as u32)
                .clamp(MAX_STEPS_MIN, MAX_STEPS_MAX)
                .max(
                    (row.max_same_tool_calls_per_turn.max(0) as u32)
                        .clamp(MAX_SAME_TOOL_CALLS_MIN, MAX_SAME_TOOL_CALLS_MAX),
                ),
            exec_approval_timeout_secs,
        }
    }

    fn into_active_model(self) -> Result<model_provider::ActiveModel, DbErr> {
        let profile = self
            .request_profile()
            .map_err(|error| DbErr::Custom(error.to_string()))?;
        let protocol = self.wire_protocol.ok_or_else(|| {
            DbErr::Custom("wire_protocol is required before saving model configuration".into())
        })?;
        let request_options = serde_json::to_string(&profile.request_options)
            .map_err(|error| DbErr::Custom(error.to_string()))?;
        Ok(model_provider::ActiveModel {
            id: Set(SINGLETON_ID),
            wire_protocol: Set(Some(protocol.to_string())),
            model: Set(self.model),
            supports_image_input: Set(self.supports_image_input),
            base_url: Set(self.base_url),
            api_key: Set(self.api_key),
            profile_schema_version: Set(i32::from(profile.profile_schema_version)),
            request_options: Set(request_options),
            output_limit_field: Set(profile.output_limit_field.to_string()),
            probe_max_output_tokens: Set(profile.probe_max_output_tokens),
            runtime_max_output_tokens: Set(profile.runtime_max_output_tokens),
            max_context_bytes: Set(profile.max_context_bytes),
            connection_revision: Set(self.connection_revision),
            profile_revision: Set(self.profile_revision),
            response_format: Set(enum_to_wire(&self.response_format)),
            execution_mode: Set(enum_to_wire(&self.execution_mode)),
            max_same_tool_calls_per_turn: Set(self
                .max_same_tool_calls_per_turn
                .clamp(MAX_SAME_TOOL_CALLS_MIN, MAX_SAME_TOOL_CALLS_MAX)
                as i32),
            max_steps_per_turn: Set(self
                .max_steps_per_turn
                .clamp(MAX_STEPS_MIN, MAX_STEPS_MAX)
                .max(
                    self.max_same_tool_calls_per_turn
                        .clamp(MAX_SAME_TOOL_CALLS_MIN, MAX_SAME_TOOL_CALLS_MAX),
                ) as i32),
            exec_approval_timeout_secs: Set(self.exec_approval_timeout_secs.clamp(
                EXEC_APPROVAL_TIMEOUT_MIN_SECS,
                EXEC_APPROVAL_TIMEOUT_MAX_SECS,
            ) as i32),
            updated_at: Set(chrono::Utc::now()),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelDestinationError {
    NotConfigured,
    InvalidRevision,
}

impl fmt::Display for ModelDestinationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotConfigured => f.write_str("OSS AI gateway is not fully configured"),
            Self::InvalidRevision => f.write_str("OSS AI gateway revision is invalid"),
        }
    }
}

impl std::error::Error for ModelDestinationError {}

impl fmt::Debug for ModelProviderConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModelProviderConfig")
            .field("wire_protocol", &self.wire_protocol)
            .field("model", &self.model)
            .field("supports_image_input", &self.supports_image_input)
            .field("base_url", &self.base_url)
            // Redact: report presence, never the value.
            .field("api_key", &self.api_key.as_ref().map(|_| "***"))
            .field("max_context_bytes", &self.max_context_bytes)
            .field("profile_schema_version", &self.profile_schema_version)
            .field("request_options", &self.request_options)
            .field("output_limit_field", &self.output_limit_field)
            .field("probe_max_output_tokens", &self.probe_max_output_tokens)
            .field("runtime_max_output_tokens", &self.runtime_max_output_tokens)
            .field("connection_revision", &self.connection_revision)
            .field("profile_revision", &self.profile_revision)
            .field("response_format", &self.response_format)
            .field("execution_mode", &self.execution_mode)
            .field(
                "max_same_tool_calls_per_turn",
                &self.max_same_tool_calls_per_turn,
            )
            .field("max_steps_per_turn", &self.max_steps_per_turn)
            .field(
                "exec_approval_timeout_secs",
                &self.exec_approval_timeout_secs,
            )
            .finish()
    }
}

/// Masked public view returned by the provider-config query endpoint. Carries no
/// secret: only whether a key is configured (`api_key_set`).
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct ModelProviderPublic {
    #[schema(value_type = Option<String>)]
    pub wire_protocol: Option<WireProtocol>,
    pub model: Option<String>,
    pub supports_image_input: bool,
    pub base_url: Option<String>,
    #[schema(minimum = 4096, maximum = 16777216)]
    pub max_context_bytes: Option<i64>,
    pub profile_schema_version: u16,
    #[schema(value_type = Object)]
    pub request_options: serde_json::Value,
    #[schema(value_type = String)]
    pub output_limit_field: OutputLimitField,
    pub probe_max_output_tokens: i64,
    pub runtime_max_output_tokens: i64,
    pub connection_revision: i64,
    pub profile_revision: i64,
    pub probe_observation: Option<ModelProbeObservation>,
    pub response_format: ResponseFormatMode,
    pub execution_mode: ExecutionMode,
    #[schema(minimum = 1, maximum = 50)]
    pub max_same_tool_calls_per_turn: u32,
    #[schema(minimum = 1, maximum = 80)]
    pub max_steps_per_turn: u32,
    #[schema(minimum = 30, maximum = 1800)]
    pub exec_approval_timeout_secs: u32,
    /// Whether a non-empty API key is configured. The key itself is never
    /// returned.
    pub api_key_set: bool,
}

impl Default for ModelProviderPublic {
    fn default() -> Self {
        ModelProviderConfig::default().public_view()
    }
}

/// Update body for the provider-config update endpoint.
///
/// Configuration fields are optional: `None` leaves the stored value unchanged.
/// The update API separately requires both expected revisions. `api_key` is
/// write-only with three-way semantics (see [`ModelProviderConfig::apply_update`]).
#[derive(Clone, Serialize, Deserialize, ToSchema, Default)]
pub struct ModelProviderUpdate {
    /// Optimistic-lock revision from the last GET. Required by the update API;
    /// it is not itself persisted as a client-selected value.
    pub expected_connection_revision: Option<i64>,
    /// Optimistic-lock revision from the last GET. Required by the update API;
    /// it is not itself persisted as a client-selected value.
    pub expected_profile_revision: Option<i64>,
    #[schema(value_type = Option<String>)]
    pub wire_protocol: Option<WireProtocol>,
    pub model: Option<String>,
    /// `None` leaves the stored image-input capability unchanged.
    pub supports_image_input: Option<bool>,
    pub base_url: Option<String>,
    #[schema(minimum = 4096, maximum = 16777216)]
    pub max_context_bytes: Option<i64>,
    #[schema(value_type = Object)]
    pub request_options: Option<serde_json::Value>,
    #[schema(value_type = Option<String>)]
    pub output_limit_field: Option<OutputLimitField>,
    pub probe_max_output_tokens: Option<i64>,
    pub runtime_max_output_tokens: Option<i64>,
    /// `None` leaves the stored format unchanged.
    pub response_format: Option<ResponseFormatMode>,
    /// `None` leaves the stored grant unchanged. A not-yet-selectable mode
    /// (`session_approved` / `automated`) is ignored.
    pub execution_mode: Option<ExecutionMode>,
    /// Per-turn cap for calls to the same tool name. Valid range: 1..=50.
    #[schema(minimum = 1, maximum = 50)]
    pub max_same_tool_calls_per_turn: Option<u32>,
    /// Per-turn model reasoning-round budget. Must be at least the same-tool
    /// repeat limit. Valid range: 1..=80.
    #[schema(minimum = 1, maximum = 80)]
    pub max_steps_per_turn: Option<u32>,
    /// Owner-confirmed command approval window. Valid range: 30..=1800 seconds.
    #[schema(minimum = 30, maximum = 1800)]
    pub exec_approval_timeout_secs: Option<u32>,
    /// Write-only. `None` = leave unchanged; `Some("")` = clear; `Some(x)` = set.
    pub api_key: Option<String>,
}

impl fmt::Debug for ModelProviderUpdate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModelProviderUpdate")
            .field("wire_protocol", &self.wire_protocol)
            .field("model", &self.model)
            .field("supports_image_input", &self.supports_image_input)
            .field("base_url", &self.base_url)
            .field("max_context_bytes", &self.max_context_bytes)
            .field("request_options", &self.request_options)
            .field("output_limit_field", &self.output_limit_field)
            .field("probe_max_output_tokens", &self.probe_max_output_tokens)
            .field("runtime_max_output_tokens", &self.runtime_max_output_tokens)
            .field("response_format", &self.response_format)
            .field("execution_mode", &self.execution_mode)
            .field(
                "max_same_tool_calls_per_turn",
                &self.max_same_tool_calls_per_turn,
            )
            .field("max_steps_per_turn", &self.max_steps_per_turn)
            .field(
                "exec_approval_timeout_secs",
                &self.exec_approval_timeout_secs,
            )
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
    let Some(row) = row else {
        return Ok(ModelProviderConfig::default());
    };
    let mut config = ModelProviderConfig::from_entity(row);
    let observation = model_probe_observation::Entity::find_by_id(SINGLETON_ID)
        .one(db)
        .await?;
    config.probe_observation = observation.and_then(|row| {
        let validated_capabilities = serde_json::from_str(&row.validated_capabilities).ok()?;
        Some(ModelProbeObservation {
            connection_revision: row.connection_revision,
            profile_revision: row.profile_revision,
            tested_at: row.tested_at,
            reasoning_observed: row.reasoning_observed.unwrap_or(false),
            reasoning_tokens: row.reasoning_tokens,
            stop_reason: row.stop_reason,
            validated_capabilities,
            current: row.connection_revision == config.connection_revision
                && row.profile_revision == config.profile_revision,
        })
    });
    Ok(config)
}

/// Persist the singleton provider config (insert-or-replace on the fixed PK).
pub async fn save(db: &DatabaseConnection, config: ModelProviderConfig) -> Result<(), DbErr> {
    let active = config.into_active_model()?;
    model_provider::Entity::insert(active)
        .on_conflict(
            OnConflict::column(model_provider::Column::Id)
                .update_columns([
                    model_provider::Column::WireProtocol,
                    model_provider::Column::Model,
                    model_provider::Column::SupportsImageInput,
                    model_provider::Column::BaseUrl,
                    model_provider::Column::ApiKey,
                    model_provider::Column::MaxContextBytes,
                    model_provider::Column::ProfileSchemaVersion,
                    model_provider::Column::RequestOptions,
                    model_provider::Column::OutputLimitField,
                    model_provider::Column::ProbeMaxOutputTokens,
                    model_provider::Column::RuntimeMaxOutputTokens,
                    model_provider::Column::ConnectionRevision,
                    model_provider::Column::ProfileRevision,
                    model_provider::Column::ResponseFormat,
                    model_provider::Column::ExecutionMode,
                    model_provider::Column::MaxSameToolCallsPerTurn,
                    model_provider::Column::MaxStepsPerTurn,
                    model_provider::Column::ExecApprovalTimeoutSecs,
                    model_provider::Column::UpdatedAt,
                ])
                .to_owned(),
        )
        .exec(db)
        .await?;
    Ok(())
}

/// Persist a configuration only when the singleton still has the revisions the
/// caller read. The conditional UPDATE is the CAS for an existing row. A fresh
/// database has no singleton row, so the insert uses DO NOTHING to make two
/// first writers race safely; only the winner reports success.
pub async fn save_if_revisions_match(
    db: &DatabaseConnection,
    config: ModelProviderConfig,
    expected_connection_revision: i64,
    expected_profile_revision: i64,
) -> Result<bool, DbErr> {
    let active = config.into_active_model()?;
    let updated = model_provider::Entity::update_many()
        .set(active.clone())
        .filter(model_provider::Column::Id.eq(SINGLETON_ID))
        .filter(model_provider::Column::ConnectionRevision.eq(expected_connection_revision))
        .filter(model_provider::Column::ProfileRevision.eq(expected_profile_revision))
        .exec(db)
        .await?;
    if updated.rows_affected == 1 {
        return Ok(true);
    }

    if expected_connection_revision != 1 || expected_profile_revision != 1 {
        return Ok(false);
    }
    let inserted = model_provider::Entity::insert(active)
        .on_conflict_do_nothing()
        .exec_without_returning(db)
        .await?;
    Ok(matches!(inserted, TryInsertResult::Inserted(1)))
}

/// Persist an observation only if both saved revisions still match the probe
/// snapshot. The no-op provider update is the first write in the transaction,
/// so SQLite serializes this CAS with concurrent configuration saves.
pub async fn save_probe_observation_if_current(
    db: &DatabaseConnection,
    observation: ModelProbeObservation,
) -> Result<bool, DbErr> {
    let validated_capabilities = serde_json::to_string(&observation.validated_capabilities)
        .map_err(|error| DbErr::Custom(error.to_string()))?;
    let txn = db.begin().await?;
    let matched = model_provider::Entity::update_many()
        .col_expr(
            model_provider::Column::Id,
            Expr::col(model_provider::Column::Id),
        )
        .filter(model_provider::Column::Id.eq(SINGLETON_ID))
        .filter(model_provider::Column::ConnectionRevision.eq(observation.connection_revision))
        .filter(model_provider::Column::ProfileRevision.eq(observation.profile_revision))
        .exec(&txn)
        .await?
        .rows_affected
        == 1;
    if !matched {
        txn.rollback().await?;
        return Ok(false);
    }
    model_probe_observation::Entity::insert(model_probe_observation::ActiveModel {
        model_provider_id: Set(SINGLETON_ID),
        connection_revision: Set(observation.connection_revision),
        profile_revision: Set(observation.profile_revision),
        tested_at: Set(observation.tested_at),
        reasoning_observed: Set(Some(observation.reasoning_observed)),
        reasoning_tokens: Set(observation.reasoning_tokens),
        stop_reason: Set(observation.stop_reason),
        validated_capabilities: Set(validated_capabilities),
    })
    .on_conflict(
        OnConflict::column(model_probe_observation::Column::ModelProviderId)
            .update_columns([
                model_probe_observation::Column::ConnectionRevision,
                model_probe_observation::Column::ProfileRevision,
                model_probe_observation::Column::TestedAt,
                model_probe_observation::Column::ReasoningObserved,
                model_probe_observation::Column::ReasoningTokens,
                model_probe_observation::Column::StopReason,
                model_probe_observation::Column::ValidatedCapabilities,
            ])
            .to_owned(),
    )
    .exec(&txn)
    .await?;
    txn.commit().await?;
    Ok(true)
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
        let stmt = schema.create_table_from_entity(model_probe_observation::Entity);
        db.execute(&stmt).await.unwrap();
        db
    }

    fn configured() -> ModelProviderConfig {
        ModelProviderConfig {
            wire_protocol: Some(WireProtocol::OpenAiChatCompletions),
            model: Some("example-model".into()),
            supports_image_input: true,
            base_url: Some("https://api.example/v1".into()),
            api_key: Some("sk-secret-value".into()),
            max_context_bytes: Some(131_072),
            profile_schema_version: MODEL_PROFILE_SCHEMA_VERSION,
            request_options: serde_json::json!({}),
            output_limit_field: OutputLimitField::MaxTokens,
            probe_max_output_tokens: 512,
            runtime_max_output_tokens: 4096,
            connection_revision: 1,
            profile_revision: 1,
            probe_observation: None,
            response_format: ResponseFormatMode::JsonObject,
            execution_mode: ExecutionMode::SuggestOnly,
            max_same_tool_calls_per_turn: MAX_SAME_TOOL_CALLS_DEFAULT,
            max_steps_per_turn: MAX_STEPS_DEFAULT,
            exec_approval_timeout_secs: EXEC_APPROVAL_TIMEOUT_DEFAULT_SECS,
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
    fn destination_identity_reuses_gateway_revisions_without_secrets_or_url() {
        let destination = configured().destination_identity().unwrap();
        assert_eq!(
            destination,
            DestinationIdentity::Model {
                connection_id: format!("oss-ai-gateway:{SINGLETON_ID}"),
                connection_revision: 1,
                model_id: "example-model".into(),
                profile_revision: 1,
            }
        );
        let json = serde_json::to_string(&destination).unwrap();
        assert!(!json.contains("sk-secret-value"));
        assert!(!json.contains("api.example"));
        assert_eq!(
            ModelProviderConfig::default().destination_identity(),
            Err(ModelDestinationError::NotConfigured)
        );
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
        assert_eq!(cfg.execution_mode, ExecutionMode::ConfirmEachAction);
        assert_eq!(
            cfg.max_same_tool_calls_per_turn,
            MAX_SAME_TOOL_CALLS_DEFAULT
        );
        assert_eq!(cfg.max_steps_per_turn, MAX_STEPS_DEFAULT);
        assert_eq!(
            cfg.exec_approval_timeout_secs,
            EXEC_APPROVAL_TIMEOUT_DEFAULT_SECS
        );
    }

    #[tokio::test]
    async fn save_then_load_round_trips_including_enums() {
        let db = memory_db().await;
        let mut cfg = configured();
        cfg.response_format = ResponseFormatMode::JsonSchema;
        cfg.execution_mode = ExecutionMode::ConfirmEachAction;
        cfg.max_same_tool_calls_per_turn = 17;
        cfg.max_steps_per_turn = 23;
        cfg.exec_approval_timeout_secs = 300;
        save(&db, cfg).await.unwrap();

        let loaded = load(&db).await.unwrap();
        assert_eq!(loaded.model.as_deref(), Some("example-model"));
        assert!(loaded.supports_image_input);
        assert_eq!(loaded.api_key.as_deref(), Some("sk-secret-value"));
        assert_eq!(loaded.max_context_bytes, Some(131_072));
        assert_eq!(loaded.response_format, ResponseFormatMode::JsonSchema);
        assert_eq!(loaded.execution_mode, ExecutionMode::ConfirmEachAction);
        assert_eq!(loaded.max_same_tool_calls_per_turn, 17);
        assert_eq!(loaded.max_steps_per_turn, 23);
        assert_eq!(loaded.exec_approval_timeout_secs, 300);
    }

    #[test]
    fn update_exec_approval_timeout_is_bounded_defensively() {
        let mut cfg = configured();
        cfg.apply_update(ModelProviderUpdate {
            exec_approval_timeout_secs: Some(0),
            ..Default::default()
        });
        assert_eq!(
            cfg.exec_approval_timeout_secs,
            EXEC_APPROVAL_TIMEOUT_MIN_SECS
        );
        cfg.apply_update(ModelProviderUpdate {
            exec_approval_timeout_secs: Some(EXEC_APPROVAL_TIMEOUT_MAX_SECS + 1),
            ..Default::default()
        });
        assert_eq!(
            cfg.exec_approval_timeout_secs,
            EXEC_APPROVAL_TIMEOUT_MAX_SECS
        );
    }

    #[test]
    fn update_same_tool_limit_is_bounded_defensively() {
        let mut cfg = configured();
        cfg.apply_update(ModelProviderUpdate {
            max_same_tool_calls_per_turn: Some(0),
            ..Default::default()
        });
        assert_eq!(cfg.max_same_tool_calls_per_turn, MAX_SAME_TOOL_CALLS_MIN);
        cfg.apply_update(ModelProviderUpdate {
            max_same_tool_calls_per_turn: Some(MAX_SAME_TOOL_CALLS_MAX + 1),
            ..Default::default()
        });
        assert_eq!(cfg.max_same_tool_calls_per_turn, MAX_SAME_TOOL_CALLS_MAX);
        assert_eq!(cfg.max_steps_per_turn, MAX_SAME_TOOL_CALLS_MAX);
    }

    #[test]
    fn update_step_budget_is_bounded_and_not_below_same_tool_limit() {
        assert!(step_budget_covers_same_tool_limit(20, 10));
        assert!(!step_budget_covers_same_tool_limit(9, 10));

        let mut cfg = configured();
        cfg.apply_update(ModelProviderUpdate {
            max_same_tool_calls_per_turn: Some(18),
            max_steps_per_turn: Some(5),
            ..Default::default()
        });
        assert_eq!(cfg.max_same_tool_calls_per_turn, 18);
        assert_eq!(cfg.max_steps_per_turn, 18);

        cfg.apply_update(ModelProviderUpdate {
            max_steps_per_turn: Some(MAX_STEPS_MAX + 1),
            ..Default::default()
        });
        assert_eq!(cfg.max_steps_per_turn, MAX_STEPS_MAX);
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

    #[tokio::test]
    async fn revision_cas_rejects_stale_updates_and_first_writer_loses_no_data() {
        let db = memory_db().await;
        let mut first = configured();
        first.apply_update(ModelProviderUpdate {
            model: Some("first-model".into()),
            ..Default::default()
        });
        let mut second = configured();
        second.apply_update(ModelProviderUpdate {
            model: Some("second-model".into()),
            ..Default::default()
        });

        assert!(save_if_revisions_match(&db, first, 1, 1).await.unwrap());
        assert!(!save_if_revisions_match(&db, second, 1, 1).await.unwrap());
        let saved = load(&db).await.unwrap();
        assert_eq!(saved.model.as_deref(), Some("first-model"));
        assert_eq!(saved.profile_revision, 2);

        let mut current = saved.clone();
        current.apply_update(ModelProviderUpdate {
            runtime_max_output_tokens: Some(8192),
            ..Default::default()
        });
        assert!(
            save_if_revisions_match(
                &db,
                current,
                saved.connection_revision,
                saved.profile_revision,
            )
            .await
            .unwrap()
        );
        assert_eq!(load(&db).await.unwrap().profile_revision, 3);
    }

    #[tokio::test]
    async fn probe_observation_is_revision_bound_and_remains_visible_when_stale() {
        let db = memory_db().await;
        save(&db, configured()).await.unwrap();
        let observation = ModelProbeObservation {
            connection_revision: 1,
            profile_revision: 1,
            tested_at: chrono::Utc::now(),
            reasoning_observed: true,
            reasoning_tokens: Some(12),
            stop_reason: Some("end_turn".to_string()),
            validated_capabilities: serde_json::json!({"text": true}),
            current: true,
        };
        assert!(
            save_probe_observation_if_current(&db, observation.clone())
                .await
                .unwrap()
        );
        assert!(load(&db).await.unwrap().probe_observation.unwrap().current);

        let mut edited = load(&db).await.unwrap();
        edited.apply_update(ModelProviderUpdate {
            request_options: Some(serde_json::json!({"reasoning_effort": "low"})),
            ..Default::default()
        });
        save(&db, edited).await.unwrap();
        let loaded = load(&db).await.unwrap();
        let stale = loaded
            .probe_observation
            .expect("stale history remains visible");
        assert!(!stale.current);
        assert!(
            !save_probe_observation_if_current(&db, observation)
                .await
                .unwrap()
        );
    }
}
