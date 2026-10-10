//! Model-provider configuration for the OSS signal central brain, with a hard
//! secret boundary mirroring the edge's `ai_model` settings.
//!
//! The signal server is the central brain in the OSS "thin edge + central brain"
//! split: it owns the model credentials and dials the provider, while edges only
//! offer device-capability interfaces. Because the portable signal server is
//! single-node and single-account, there is exactly one provider config
//! (persisted in the global configuration file).
//!
//! The secret boundary has three faces (matching the edge `ai_model` design):
//! - [`ModelProviderConfig`] — the loaded form. `api_key` is plaintext in the
//!   local configuration file but its [`std::fmt::Debug`] is redacted.
//! - [`ModelProviderPublic`] — what `GET` returns. It reports only whether a key
//!   is configured (`api_key_set`), never the key itself.
//! - [`ModelProviderUpdate`] — what `POST` accepts. `api_key` is write-only with
//!   explicit leave / clear / set semantics.

use crate::config::connection::DatabaseConnection;
use std::fmt;

use desk_agent_protocol::ExecutionMode;
use desk_agent_protocol::data_lineage::DestinationIdentity;
use desk_diagnose_core::model_profile::{
    DEFAULT_RUNTIME_MAX_OUTPUT_TOKENS, MODEL_PROFILE_SCHEMA_VERSION, ModelRequestProfile,
    OutputLimitField, ProfileError, ReasoningContract, WireProtocol,
};
use sea_orm::ActiveValue::Set;
use sea_orm::sea_query::OnConflict;
use sea_orm::{DbErr, EntityTrait};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

const SINGLETON_ID: i32 = 1;
use crate::config::ConfigConnection;
use crate::entity::model_probe_observation;

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

/// Loaded model-provider configuration (the central brain's credentials + policy
/// defaults).
///
/// `Debug` is implemented by hand so `api_key` is never rendered.
#[derive(Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
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
    pub reasoning_contract: ReasoningContract,
    pub anthropic_prefix_binding: bool,
    pub profile_schema_version: u16,
    pub request_options: serde_json::Value,
    pub output_limit_field: OutputLimitField,
    pub runtime_max_output_tokens: i64,
    /// Explicit local model-history budget. It is not inferred from the model.
    pub max_context_bytes: Option<i64>,
    pub connection_revision: i64,
    pub profile_revision: i64,
    #[serde(skip)]
    pub config_instance: String,
    #[serde(skip)]
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
            reasoning_contract: Default::default(),
            anthropic_prefix_binding: false,
            profile_schema_version: MODEL_PROFILE_SCHEMA_VERSION,
            request_options: serde_json::json!({}),
            output_limit_field: OutputLimitField::MaxTokens,
            runtime_max_output_tokens: DEFAULT_RUNTIME_MAX_OUTPUT_TOKENS,
            max_context_bytes: None,
            connection_revision: 1,
            profile_revision: 1,
            config_instance: String::new(),
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
            connection_id: format!("oss-ai-gateway:{SINGLETON_ID}:{}", self.config_instance),
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
            reasoning_contract: self.reasoning_contract,
            anthropic_prefix_binding: self.anthropic_prefix_binding,
            profile_schema_version: self.profile_schema_version,
            request_options: self.request_options.clone(),
            output_limit_field: self.output_limit_field,
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
            reasoning_contract: self.reasoning_contract,
            anthropic_prefix_binding: self.anthropic_prefix_binding,
            profile_schema_version: self.profile_schema_version,
            request_options: self.request_options.clone(),
            output_limit_field: self.output_limit_field,
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
        let old_model = self.model.clone();
        let old_options = desk_diagnose_core::prompt_cache::without_cache(&self.request_options);
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
        let mut profile_changed = update
            .reasoning_contract
            .is_some_and(|value| value != self.reasoning_contract)
            || update
                .anthropic_prefix_binding
                .is_some_and(|value| value != self.anthropic_prefix_binding)
            || update
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
        if let Some(value) = update.reasoning_contract {
            self.reasoning_contract = value;
        }
        if let Some(value) = update.anthropic_prefix_binding {
            self.anthropic_prefix_binding = value;
        }
        if let Some(request_options) = update.request_options {
            self.request_options = request_options;
        }
        if let Some(output_limit_field) = update.output_limit_field {
            self.output_limit_field = output_limit_field;
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
            profile_changed |=
                desk_diagnose_core::prompt_cache::reset_connection(&mut self.request_options);
        } else if old_model != self.model
            || old_options != desk_diagnose_core::prompt_cache::without_cache(&self.request_options)
        {
            profile_changed |=
                desk_diagnose_core::prompt_cache::reset_history(&mut self.request_options);
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
    #[schema(value_type = String)]
    pub reasoning_contract: ReasoningContract,
    pub anthropic_prefix_binding: bool,
    pub profile_schema_version: u16,
    #[schema(value_type = Object)]
    pub request_options: serde_json::Value,
    #[schema(value_type = String)]
    pub output_limit_field: OutputLimitField,
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
    pub reasoning_contract: Option<ReasoningContract>,
    pub anthropic_prefix_binding: Option<bool>,
    #[schema(value_type = Option<String>)]
    pub output_limit_field: Option<OutputLimitField>,
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

/// Load file settings and attach the independently stored probe observation.
pub async fn load<C: crate::config::ConfigConnection>(
    db: &C,
) -> Result<ModelProviderConfig, DbErr> {
    let snapshot = db.config_read().await;
    let mut config = snapshot.ai_gateway.clone();
    config.config_instance = snapshot.metadata.instance.clone();
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
            current: row.config_instance == snapshot.metadata.instance
                && row.connection_revision == config.connection_revision
                && row.profile_revision == config.profile_revision,
        })
    });
    Ok(config)
}

pub async fn save(db: &DatabaseConnection, config: ModelProviderConfig) -> Result<(), DbErr> {
    config
        .request_profile()
        .map_err(|error| DbErr::Custom(error.to_string()))?;
    db.config_context()
        .update::<_, DbErr, _>(|candidate| {
            config
                .request_profile()
                .map_err(|error| DbErr::Custom(error.to_string()))?
                .validate_for_use_case(
                    config
                        .wire_protocol
                        .ok_or_else(|| DbErr::Custom("wire protocol is required".into()))?,
                    desk_diagnose_core::model_profile::ModelUseCase::Completion,
                    Some(i64::from(candidate.terminal_completion.max_output_tokens)),
                )
                .map_err(|error| DbErr::Custom(error.to_string()))?;
            candidate.ai_gateway = config;
            Ok(Some(()))
        })
        .await?;
    Ok(())
}

#[derive(Debug)]
pub enum ModelConfigWriteError {
    Invalid(String),
    Db(DbErr),
}
impl From<DbErr> for ModelConfigWriteError {
    fn from(error: DbErr) -> Self {
        Self::Db(error)
    }
}

pub async fn save_if_revisions_match(
    db: &DatabaseConnection,
    config: ModelProviderConfig,
    expected_connection_revision: i64,
    expected_profile_revision: i64,
) -> Result<bool, ModelConfigWriteError> {
    config
        .request_profile()
        .map_err(|error| DbErr::Custom(error.to_string()))?;
    Ok(db
        .config_context()
        .update::<_, ModelConfigWriteError, _>(|candidate| {
            if candidate.ai_gateway.connection_revision != expected_connection_revision
                || candidate.ai_gateway.profile_revision != expected_profile_revision
            {
                return Ok(None);
            }
            config
                .request_profile()
                .map_err(|error| ModelConfigWriteError::Invalid(error.to_string()))?
                .validate_for_use_case(
                    config.wire_protocol.ok_or_else(|| {
                        ModelConfigWriteError::Invalid("wire protocol is required".into())
                    })?,
                    desk_diagnose_core::model_profile::ModelUseCase::Completion,
                    Some(i64::from(candidate.terminal_completion.max_output_tokens)),
                )
                .map_err(|error| ModelConfigWriteError::Invalid(error.to_string()))?;
            candidate.ai_gateway = config;
            Ok(Some(()))
        })
        .await?
        .is_some())
}

/// The transaction pins the file identity before taking the SQLite writer.
pub async fn save_probe_observation_if_current(
    db: &DatabaseConnection,
    expected_instance: &str,
    observation: ModelProbeObservation,
) -> Result<bool, DbErr> {
    let txn = crate::db::begin_write(db, model_probe_observation::Entity).await?;
    let snapshot = txn.config_read().await;
    if snapshot.metadata.instance != expected_instance
        || snapshot.ai_gateway.connection_revision != observation.connection_revision
        || snapshot.ai_gateway.profile_revision != observation.profile_revision
    {
        drop(snapshot);
        txn.rollback().await?;
        return Ok(false);
    }
    model_probe_observation::Entity::insert(model_probe_observation::ActiveModel {
        model_provider_id: Set(SINGLETON_ID),
        config_instance: Set(snapshot.metadata.instance.clone()),
        connection_revision: Set(observation.connection_revision),
        profile_revision: Set(observation.profile_revision),
        tested_at: Set(observation.tested_at),
        reasoning_observed: Set(Some(observation.reasoning_observed)),
        reasoning_tokens: Set(observation.reasoning_tokens),
        stop_reason: Set(observation.stop_reason),
        validated_capabilities: Set(observation.validated_capabilities.to_string()),
    })
    .on_conflict(
        OnConflict::column(model_probe_observation::Column::ModelProviderId)
            .update_columns([
                model_probe_observation::Column::ConfigInstance,
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
    drop(snapshot);
    txn.commit().await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::{ConnectionTrait, Schema};

    async fn memory_db() -> DatabaseConnection {
        let db = crate::config::test_support::Database::connect("sqlite::memory:")
            .await
            .unwrap();
        let schema = Schema::new(db.get_database_backend());
        let stmt = schema.create_table_from_entity(model_probe_observation::Entity);
        db.execute(&stmt).await.unwrap();
        db
    }

    fn configured() -> ModelProviderConfig {
        ModelProviderConfig {
            config_instance: String::new(),
            wire_protocol: Some(WireProtocol::OpenAiChatCompletions),
            model: Some("example-model".into()),
            supports_image_input: true,
            base_url: Some("https://api.example/v1".into()),
            api_key: Some("sk-secret-value".into()),
            max_context_bytes: Some(131_072),
            reasoning_contract: Default::default(),
            anthropic_prefix_binding: false,
            profile_schema_version: MODEL_PROFILE_SCHEMA_VERSION,
            request_options: serde_json::json!({}),
            output_limit_field: OutputLimitField::MaxTokens,
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
                connection_id: format!(
                    "oss-ai-gateway:{SINGLETON_ID}:{}",
                    configured().config_instance
                ),
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
        assert_eq!(
            load(&db).await.unwrap().model.as_deref(),
            Some("other-model")
        );
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
    async fn model_save_validates_completion_against_the_policy_inside_the_file_write() {
        let db = memory_db().await;
        let mut config = configured();
        config.wire_protocol = Some(WireProtocol::AnthropicMessages);
        config.runtime_max_output_tokens = 32768;
        config.request_options =
            serde_json::json!({"thinking":{"type":"enabled","budget_tokens":1024}});
        assert!(matches!(
            save_if_revisions_match(&db, config.clone(), 1, 1).await,
            Err(ModelConfigWriteError::Invalid(_))
        ));
        crate::terminal_completion_config::update(
            &db,
            &desk_signal_facade::terminal_completion::UpdateTerminalCompletionRequest {
                expected_revision: 0,
                max_output_tokens: 32768,
            },
        )
        .await
        .unwrap();
        assert!(
            save_if_revisions_match(&db, config.clone(), 1, 1)
                .await
                .unwrap()
        );
        let current = load(&db).await.unwrap();
        crate::terminal_completion_config::update(
            &db,
            &desk_signal_facade::terminal_completion::UpdateTerminalCompletionRequest {
                expected_revision: 1,
                max_output_tokens: 512,
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            save_if_revisions_match(
                &db,
                config,
                current.connection_revision,
                current.profile_revision
            )
            .await,
            Err(ModelConfigWriteError::Invalid(_))
        ));
        assert_eq!(
            load(&db).await.unwrap().profile_revision,
            current.profile_revision
        );
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
            save_probe_observation_if_current(
                &db,
                &db.config_context().instance().await,
                observation.clone()
            )
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
            !save_probe_observation_if_current(
                &db,
                &db.config_context().instance().await,
                observation
            )
            .await
            .unwrap()
        );
    }
}
