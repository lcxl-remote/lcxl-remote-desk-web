//! Model request profiles shared by the manager and the OSS signal runtime.
//!
//! A profile describes a provider wire contract and explicitly configured
//! request behavior. It deliberately contains no model-name or vendor-name
//! inference: compatible endpoints may expose arbitrary model names, so the
//! configuration is the source of truth.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::{MAX_MODEL_CONTEXT_BYTES, MIN_MODEL_CONTEXT_BYTES};

pub const DEFAULT_RUNTIME_MAX_OUTPUT_TOKENS: i64 = 64 * 1024;

pub const MODEL_PROFILE_SCHEMA_VERSION: u16 = 2;

const OUTPUT_LIMIT_KEYS: [&str; 3] = ["max_tokens", "max_completion_tokens", "max_output_tokens"];

const RESERVED_OPTION_KEYS: [&str; 13] = [
    "model",
    "messages",
    "system",
    "stream",
    "stream_options",
    "tools",
    "tool_choice",
    "max_tokens",
    "max_completion_tokens",
    "max_output_tokens",
    "temperature",
    "top_p",
    "top_k",
];

/// Provider request/response contract. Values identify protocols, not vendors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireProtocol {
    OpenAiChatCompletions,
    AnthropicMessages,
    /// Reserved for a future adapter. It must never be rendered as Chat
    /// Completions merely because both protocols are associated with OpenAI.
    OpenAiResponses,
}

impl WireProtocol {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiChatCompletions => "open_ai_chat_completions",
            Self::AnthropicMessages => "anthropic_messages",
            Self::OpenAiResponses => "open_ai_responses",
        }
    }

    /// Parse a persisted/API value without aliases or empty-value defaults.
    pub fn parse(value: &str) -> Result<Self, ProfileError> {
        value.parse()
    }
}

impl fmt::Display for WireProtocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for WireProtocol {
    type Err = ProfileError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "open_ai_chat_completions" => Ok(Self::OpenAiChatCompletions),
            "anthropic_messages" => Ok(Self::AnthropicMessages),
            "open_ai_responses" => Ok(Self::OpenAiResponses),
            _ => Err(ProfileError::UnknownWireProtocol(value.to_string())),
        }
    }
}

/// The single provider-body field used for the output-token ceiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputLimitField {
    MaxTokens,
    MaxCompletionTokens,
    MaxOutputTokens,
}

impl OutputLimitField {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MaxTokens => "max_tokens",
            Self::MaxCompletionTokens => "max_completion_tokens",
            Self::MaxOutputTokens => "max_output_tokens",
        }
    }

    pub fn parse(value: &str) -> Result<Self, ProfileError> {
        value.parse()
    }

    /// Read a positive limit while rejecting conflicting output-limit aliases.
    pub fn read_positive(self, body: &Value) -> Result<PositiveOutputLimit, ProfileError> {
        let object = body.as_object().ok_or(ProfileError::BodyMustBeObject)?;
        self.reject_conflicting_aliases(object)?;
        let value = object
            .get(self.as_str())
            .and_then(Value::as_i64)
            .ok_or(ProfileError::MissingOrInvalidOutputLimit(self.as_str()))?;
        PositiveOutputLimit::new(value)
    }

    /// Set the selected limit while rejecting any other output-limit alias.
    pub fn set_positive(
        self,
        body: &mut Value,
        value: PositiveOutputLimit,
    ) -> Result<(), ProfileError> {
        let object = body.as_object_mut().ok_or(ProfileError::BodyMustBeObject)?;
        self.reject_conflicting_aliases(object)?;
        object.insert(self.as_str().to_string(), Value::from(value.get()));
        Ok(())
    }

    fn reject_conflicting_aliases(self, body: &Map<String, Value>) -> Result<(), ProfileError> {
        for key in OUTPUT_LIMIT_KEYS {
            if key != self.as_str() && body.contains_key(key) {
                return Err(ProfileError::ConflictingOutputLimitField(key));
            }
        }
        Ok(())
    }
}

impl fmt::Display for OutputLimitField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for OutputLimitField {
    type Err = ProfileError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "max_tokens" => Ok(Self::MaxTokens),
            "max_completion_tokens" => Ok(Self::MaxCompletionTokens),
            "max_output_tokens" => Ok(Self::MaxOutputTokens),
            _ => Err(ProfileError::UnknownOutputLimitField(value.to_string())),
        }
    }
}

/// Purpose of a model request, including observation and caller-cap rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModelUseCase {
    Probe,
    Safety,
    Approval,
    Agent,
    Completion,
    FleetNaturalLanguage,
    /// Inline portable checkpoint-summary compressor.
    ContextCompression,
}

/// A validated positive provider output-token limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PositiveOutputLimit(i64);

impl PositiveOutputLimit {
    pub fn new(value: i64) -> Result<Self, ProfileError> {
        if value <= 0 {
            return Err(ProfileError::OutputLimitMustBePositive(value));
        }
        Ok(Self(value))
    }

    pub const fn get(self) -> i64 {
        self.0
    }
}

/// Explicit replay behavior; unknown gateways retain their observed material.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningContract {
    #[default]
    Conservative,
    OpenaiChat,
    DeepseekChat,
    AnthropicMessages,
}

impl ReasoningContract {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Conservative => "conservative",
            Self::OpenaiChat => "openai_chat",
            Self::DeepseekChat => "deepseek_chat",
            Self::AnthropicMessages => "anthropic_messages",
        }
    }

    pub fn parse(value: &str) -> Result<Self, ProfileError> {
        match value {
            "conservative" => Ok(Self::Conservative),
            "openai_chat" => Ok(Self::OpenaiChat),
            "deepseek_chat" => Ok(Self::DeepseekChat),
            "anthropic_messages" => Ok(Self::AnthropicMessages),
            _ => Err(ProfileError::InvalidRequestOption(
                "unknown reasoning_contract".into(),
            )),
        }
    }

    fn validate(
        self,
        protocol: WireProtocol,
        prefix: bool,
        options: &Value,
    ) -> Result<(), ProfileError> {
        let valid = match self {
            Self::Conservative => true,
            Self::OpenaiChat | Self::DeepseekChat => {
                protocol == WireProtocol::OpenAiChatCompletions
            }
            Self::AnthropicMessages => protocol == WireProtocol::AnthropicMessages,
        };
        if !valid || (prefix && self != Self::AnthropicMessages) {
            return Err(ProfileError::InvalidRequestOption(
                "reasoning_contract/prefix binding does not match the wire protocol".into(),
            ));
        }
        if self == Self::OpenaiChat && options.get("thinking").is_some() {
            return Err(ProfileError::InvalidRequestOption("thinking is a vendor extension; standard OpenAI Chat uses reasoning_effort. Choose the actual DeepSeek or conservative gateway contract when using this extension.".into()));
        }
        if let Some(effort) = options.get("reasoning_effort").and_then(Value::as_str)
            && ((self == Self::DeepseekChat && !matches!(effort, "low" | "high" | "max"))
                || (self != Self::DeepseekChat && effort == "max"))
        {
            return Err(ProfileError::InvalidRequestOption(
                "reasoning_effort is not supported by the selected contract".into(),
            ));
        }
        Ok(())
    }
}

pub const CONTEXT_MANAGEMENT_BETA: &str = "context-management-2025-06-27";

/// Stable model profile stored as relational scalar columns plus typed options.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRequestProfile {
    pub profile_schema_version: u16,
    pub reasoning_contract: ReasoningContract,
    pub anthropic_prefix_binding: bool,
    pub request_options: Value,
    pub output_limit_field: OutputLimitField,
    pub runtime_max_output_tokens: i64,
    pub max_context_bytes: i64,
    pub profile_revision: i64,
}

impl ModelRequestProfile {
    /// Validate storage-level and cross-protocol invariants.
    pub fn validate(&self, protocol: WireProtocol) -> Result<(), ProfileError> {
        if self.profile_schema_version != MODEL_PROFILE_SCHEMA_VERSION {
            return Err(ProfileError::UnsupportedProfileSchemaVersion(
                self.profile_schema_version,
            ));
        }
        if self.profile_revision < 1 {
            return Err(ProfileError::InvalidProfileRevision(self.profile_revision));
        }
        PositiveOutputLimit::new(self.runtime_max_output_tokens)?;
        let context = usize::try_from(self.max_context_bytes)
            .map_err(|_| ProfileError::InvalidMaxContextBytes(self.max_context_bytes))?;
        if !(MIN_MODEL_CONTEXT_BYTES..=MAX_MODEL_CONTEXT_BYTES).contains(&context) {
            return Err(ProfileError::InvalidMaxContextBytes(self.max_context_bytes));
        }
        if !self.request_options.is_object() {
            return Err(ProfileError::RequestOptionsMustBeObject);
        }
        validate_output_field(protocol, self.output_limit_field)?;
        self.reasoning_contract.validate(
            protocol,
            self.anthropic_prefix_binding,
            &self.request_options,
        )?;
        let options = parse_options(protocol, &self.request_options)?;
        options.validate_limits(self.runtime_max_output_tokens)
    }

    pub fn max_context_bytes(&self) -> Result<usize, ProfileError> {
        self.validate_context_bytes()
    }

    /// Validate the profile against the output ceiling that a concrete request
    /// purpose will actually receive. This catches combinations such as an
    /// Anthropic manual thinking budget that fits the runtime profile but not a
    /// completion caller's smaller hard cap.
    pub fn validate_for_use_case(
        &self,
        protocol: WireProtocol,
        use_case: ModelUseCase,
        caller_hard_cap: Option<i64>,
    ) -> Result<PositiveOutputLimit, ProfileError> {
        self.validate(protocol)?;
        let effective = resolve_effective_output_limit(
            use_case,
            self.runtime_max_output_tokens,
            caller_hard_cap,
        )?;
        parse_options(protocol, &self.request_options)?.validate_effective_limit(effective)?;
        Ok(effective)
    }

    fn validate_context_bytes(&self) -> Result<usize, ProfileError> {
        let value = usize::try_from(self.max_context_bytes)
            .map_err(|_| ProfileError::InvalidMaxContextBytes(self.max_context_bytes))?;
        if !(MIN_MODEL_CONTEXT_BYTES..=MAX_MODEL_CONTEXT_BYTES).contains(&value) {
            return Err(ProfileError::InvalidMaxContextBytes(self.max_context_bytes));
        }
        Ok(value)
    }
}

/// Tests and runtime calls share the configured output budget. A caller hard
/// cap can only narrow runtime requests; probes cannot add a caller cap.
pub fn resolve_effective_output_limit(
    use_case: ModelUseCase,
    profile_runtime_limit: i64,
    caller_hard_cap: Option<i64>,
) -> Result<PositiveOutputLimit, ProfileError> {
    if use_case == ModelUseCase::Probe && caller_hard_cap.is_some() {
        return Err(ProfileError::ProbeHardCapNotAllowed);
    }
    let profile_limit = PositiveOutputLimit::new(profile_runtime_limit)?;

    match caller_hard_cap {
        Some(cap) => {
            let cap = PositiveOutputLimit::new(cap)?;
            PositiveOutputLimit::new(profile_limit.get().min(cap.get()))
        }
        None => Ok(profile_limit),
    }
}

/// Apply a fully validated profile to a provider request body.
pub fn apply_model_request_profile(
    protocol: WireProtocol,
    use_case: ModelUseCase,
    profile: &ModelRequestProfile,
    effective_output_limit: PositiveOutputLimit,
    body: &mut Value,
) -> Result<(), ProfileError> {
    profile.validate(protocol)?;
    let expected =
        resolve_effective_output_limit(use_case, profile.runtime_max_output_tokens, None)?;
    if effective_output_limit.get() > expected.get() {
        return Err(ProfileError::EffectiveLimitExceedsProfile {
            effective: effective_output_limit.get(),
            profile: expected.get(),
        });
    }

    let options = parse_options(protocol, &profile.request_options)?;
    reject_reserved_option_keys(&profile.request_options)?;
    profile
        .output_limit_field
        .set_positive(body, effective_output_limit)?;
    options.apply(body)?;
    options.validate_effective_limit(effective_output_limit)?;
    profile.output_limit_field.read_positive(body)?;
    Ok(())
}

fn validate_output_field(
    protocol: WireProtocol,
    field: OutputLimitField,
) -> Result<(), ProfileError> {
    let valid = match protocol {
        WireProtocol::OpenAiChatCompletions => matches!(
            field,
            OutputLimitField::MaxTokens | OutputLimitField::MaxCompletionTokens
        ),
        WireProtocol::AnthropicMessages => field == OutputLimitField::MaxTokens,
        WireProtocol::OpenAiResponses => field == OutputLimitField::MaxOutputTokens,
    };
    if !valid {
        return Err(ProfileError::OutputFieldProtocolMismatch { protocol, field });
    }
    if protocol == WireProtocol::OpenAiResponses {
        return Err(ProfileError::UnsupportedWireProtocol(protocol));
    }
    Ok(())
}

fn reject_reserved_option_keys(options: &Value) -> Result<(), ProfileError> {
    let object = options
        .as_object()
        .ok_or(ProfileError::RequestOptionsMustBeObject)?;
    if let Some(key) = RESERVED_OPTION_KEYS
        .into_iter()
        .find(|key| object.contains_key(*key))
    {
        return Err(ProfileError::ReservedRequestOption(key));
    }
    Ok(())
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenAiRequestOptions {
    #[serde(default)]
    thinking: Option<OpenAiThinking>,
    #[serde(default)]
    reasoning_effort: Option<OpenAiReasoningEffort>,
    #[serde(default)]
    enable_thinking: Option<bool>,
    #[serde(default)]
    thinking_budget: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum OpenAiThinking {
    Enabled,
    Disabled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum OpenAiReasoningEffort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct AnthropicRequestOptions {
    #[serde(default)]
    prompt_cache: crate::prompt_cache::CacheOptions,
    #[serde(default)]
    thinking: Option<AnthropicThinking>,
    #[serde(default)]
    output_config: Option<AnthropicOutputConfig>,
    #[serde(default)]
    context_management: Option<ThinkingContextManagement>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ThinkingContextManagement {
    edits: Vec<ThinkingEdit>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
enum ThinkingEdit {
    #[serde(rename = "clear_thinking_20251015")]
    ClearThinking { keep: ThinkingKeep },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum ThinkingKeep {
    All(String),
    Turns(ThinkingTurns),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ThinkingTurns {
    #[serde(rename = "type")]
    kind: String,
    value: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum AnthropicThinking {
    Disabled,
    Adaptive {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display: Option<AnthropicDisplay>,
    },
    Enabled {
        budget_tokens: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display: Option<AnthropicDisplay>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum AnthropicDisplay {
    Summarized,
    Omitted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AnthropicOutputConfig {
    effort: AnthropicEffort,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum AnthropicEffort {
    Low,
    Medium,
    High,
    Max,
}

enum TypedRequestOptions {
    OpenAi(OpenAiRequestOptions),
    Anthropic(AnthropicRequestOptions),
}

impl TypedRequestOptions {
    fn validate_limits(&self, runtime: i64) -> Result<(), ProfileError> {
        match self {
            Self::OpenAi(options) => {
                if let Some(value) = options.thinking_budget
                    && value <= 0
                {
                    return Err(ProfileError::InvalidRequestOption(
                        "thinking_budget must be positive".to_string(),
                    ));
                }
            }
            Self::Anthropic(options) => {
                if let Some(context) = &options.context_management {
                    if context.edits.len() != 1 {
                        return Err(ProfileError::InvalidRequestOption(
                            "context_management requires exactly one clear_thinking_20251015 edit"
                                .into(),
                        ));
                    }
                    for ThinkingEdit::ClearThinking { keep } in &context.edits {
                        let valid = match keep {
                            ThinkingKeep::All(value) => value == "all",
                            ThinkingKeep::Turns(turns) => {
                                turns.kind == "thinking_turns" && turns.value > 0
                            }
                        };
                        if !valid {
                            return Err(ProfileError::InvalidRequestOption(
                                "thinking keep must be all or a positive thinking_turns value"
                                    .into(),
                            ));
                        }
                    }
                }
                if options.prompt_cache.cache_history
                    && options.prompt_cache.mode
                        != crate::prompt_cache::CacheMode::AnthropicExplicit
                {
                    return Err(ProfileError::InvalidRequestOption(
                        "cache_history requires anthropic_explicit mode".into(),
                    ));
                }

                if options.output_config.is_some()
                    && !matches!(options.thinking, Some(AnthropicThinking::Adaptive { .. }))
                {
                    return Err(ProfileError::InvalidRequestOption(
                        "output_config is only valid with adaptive thinking".to_string(),
                    ));
                }
                if let Some(AnthropicThinking::Enabled { budget_tokens, .. }) = options.thinking
                    && (budget_tokens <= 0 || budget_tokens >= runtime)
                {
                    return Err(ProfileError::InvalidRequestOption(format!(
                        "manual thinking budget_tokens ({budget_tokens}) must be positive and lower than the runtime ({runtime}) output limit"
                    )));
                }
            }
        }
        Ok(())
    }

    fn validate_effective_limit(&self, effective: PositiveOutputLimit) -> Result<(), ProfileError> {
        if let Self::Anthropic(options) = self
            && let Some(AnthropicThinking::Enabled { budget_tokens, .. }) = options.thinking
            && budget_tokens >= effective.get()
        {
            return Err(ProfileError::InvalidRequestOption(format!(
                "manual thinking budget_tokens ({budget_tokens}) must be lower than the effective output limit ({})",
                effective.get()
            )));
        }
        Ok(())
    }

    fn apply(&self, body: &mut Value) -> Result<(), ProfileError> {
        let object = body.as_object_mut().ok_or(ProfileError::BodyMustBeObject)?;
        match self {
            Self::OpenAi(options) => {
                if let Some(thinking) = &options.thinking {
                    object.insert("thinking".to_string(), to_value(thinking)?);
                }
                if let Some(reasoning_effort) = &options.reasoning_effort {
                    object.insert("reasoning_effort".to_string(), to_value(reasoning_effort)?);
                }
                if let Some(enable_thinking) = options.enable_thinking {
                    object.insert("enable_thinking".to_string(), Value::Bool(enable_thinking));
                }
                if let Some(thinking_budget) = options.thinking_budget {
                    object.insert("thinking_budget".to_string(), Value::from(thinking_budget));
                }
            }
            Self::Anthropic(options) => {
                match &options.thinking {
                    Some(AnthropicThinking::Disabled) | None => {}
                    Some(thinking) => {
                        object.insert("thinking".to_string(), to_value(thinking)?);
                    }
                }
                if let Some(output_config) = &options.output_config {
                    object.insert("output_config".to_string(), to_value(output_config)?);
                }
                if let Some(context) = &options.context_management {
                    object.insert("context_management".into(), to_value(context)?);
                }
            }
        }
        Ok(())
    }
}

fn parse_options(
    protocol: WireProtocol,
    value: &Value,
) -> Result<TypedRequestOptions, ProfileError> {
    if !value.is_object() {
        return Err(ProfileError::RequestOptionsMustBeObject);
    }
    reject_reserved_option_keys(value)?;
    match protocol {
        WireProtocol::OpenAiChatCompletions => serde_json::from_value(value.clone())
            .map(TypedRequestOptions::OpenAi)
            .map_err(|error| ProfileError::InvalidRequestOption(error.to_string())),
        WireProtocol::AnthropicMessages => serde_json::from_value(value.clone())
            .map(TypedRequestOptions::Anthropic)
            .map_err(|error| ProfileError::InvalidRequestOption(error.to_string())),
        WireProtocol::OpenAiResponses => Err(ProfileError::UnsupportedWireProtocol(protocol)),
    }
}

fn to_value<T: Serialize>(value: &T) -> Result<Value, ProfileError> {
    serde_json::to_value(value)
        .map_err(|error| ProfileError::InvalidRequestOption(error.to_string()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileError {
    UnknownWireProtocol(String),
    UnknownOutputLimitField(String),
    UnsupportedWireProtocol(WireProtocol),
    UnsupportedProfileSchemaVersion(u16),
    InvalidProfileRevision(i64),
    OutputLimitMustBePositive(i64),
    InvalidMaxContextBytes(i64),
    RequestOptionsMustBeObject,
    ReservedRequestOption(&'static str),
    InvalidRequestOption(String),
    OutputFieldProtocolMismatch {
        protocol: WireProtocol,
        field: OutputLimitField,
    },
    BodyMustBeObject,
    MissingOrInvalidOutputLimit(&'static str),
    ConflictingOutputLimitField(&'static str),
    ProbeHardCapNotAllowed,
    EffectiveLimitExceedsProfile {
        effective: i64,
        profile: i64,
    },
}

impl fmt::Display for ProfileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownWireProtocol(value) => write!(f, "unknown wire protocol: {value}"),
            Self::UnknownOutputLimitField(value) => {
                write!(f, "unknown output limit field: {value}")
            }
            Self::UnsupportedWireProtocol(protocol) => {
                write!(f, "wire protocol is not implemented: {protocol}")
            }
            Self::UnsupportedProfileSchemaVersion(version) => {
                write!(f, "unsupported model profile schema version: {version}")
            }
            Self::InvalidProfileRevision(revision) => {
                write!(f, "profile revision must be at least 1, got {revision}")
            }
            Self::OutputLimitMustBePositive(value) => {
                write!(f, "output limit must be positive, got {value}")
            }
            Self::InvalidMaxContextBytes(value) => write!(
                f,
                "max_context_bytes must be in {MIN_MODEL_CONTEXT_BYTES}..={MAX_MODEL_CONTEXT_BYTES}, got {value}"
            ),
            Self::RequestOptionsMustBeObject => f.write_str("request_options must be an object"),
            Self::ReservedRequestOption(key) => {
                write!(f, "request_options may not override reserved field {key}")
            }
            Self::InvalidRequestOption(detail) => write!(f, "invalid request_options: {detail}"),
            Self::OutputFieldProtocolMismatch { protocol, field } => write!(
                f,
                "output limit field {field} is not valid for wire protocol {protocol}"
            ),
            Self::BodyMustBeObject => f.write_str("provider request body must be an object"),
            Self::MissingOrInvalidOutputLimit(field) => {
                write!(f, "provider body has no positive integer {field}")
            }
            Self::ConflictingOutputLimitField(field) => {
                write!(
                    f,
                    "provider body contains conflicting output limit field {field}"
                )
            }
            Self::ProbeHardCapNotAllowed => {
                f.write_str("probe requests may not supply a caller output hard cap")
            }
            Self::EffectiveLimitExceedsProfile { effective, profile } => write!(
                f,
                "effective output limit {effective} exceeds profile limit {profile}"
            ),
        }
    }
}

impl std::error::Error for ProfileError {}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn profile(options: Value) -> ModelRequestProfile {
        ModelRequestProfile {
            reasoning_contract: Default::default(),
            anthropic_prefix_binding: false,
            profile_schema_version: MODEL_PROFILE_SCHEMA_VERSION,
            request_options: options,
            output_limit_field: OutputLimitField::MaxTokens,
            runtime_max_output_tokens: 4096,
            max_context_bytes: 131_072,
            profile_revision: 1,
        }
    }

    #[test]
    fn parsers_are_exact_and_fail_closed() {
        assert_eq!(
            WireProtocol::parse("open_ai_chat_completions").unwrap(),
            WireProtocol::OpenAiChatCompletions
        );
        assert!(WireProtocol::parse("").is_err());
        assert!(WireProtocol::parse("openai").is_err());
        assert!(OutputLimitField::parse("maxTokens").is_err());
    }

    #[test]
    fn output_field_rejects_alias_conflicts() {
        let mut body = json!({"max_completion_tokens": 20});
        let error = OutputLimitField::MaxTokens
            .set_positive(&mut body, PositiveOutputLimit::new(10).unwrap())
            .unwrap_err();
        assert_eq!(
            error,
            ProfileError::ConflictingOutputLimitField("max_completion_tokens")
        );
    }

    #[test]
    fn runtime_limit_can_only_be_narrowed() {
        assert_eq!(
            resolve_effective_output_limit(ModelUseCase::Completion, 4096, Some(512))
                .unwrap()
                .get(),
            512
        );
        assert_eq!(
            resolve_effective_output_limit(ModelUseCase::Agent, 4096, Some(8192))
                .unwrap()
                .get(),
            4096
        );
        assert!(resolve_effective_output_limit(ModelUseCase::Probe, 4096, Some(16)).is_err());
    }

    #[test]
    fn completion_validation_rejects_manual_budget_at_the_caller_cap() {
        let profile = profile(json!({
            "thinking": {"type": "enabled", "budget_tokens": 600}
        }));
        let error = profile
            .validate_for_use_case(
                WireProtocol::AnthropicMessages,
                ModelUseCase::Completion,
                Some(512),
            )
            .unwrap_err();
        assert!(error.to_string().contains("effective output limit (512)"));
    }

    #[test]
    fn compression_cap_reuses_profile_options_and_rejects_an_incompatible_manual_budget() {
        assert_eq!(
            resolve_effective_output_limit(ModelUseCase::ContextCompression, 8192, Some(4096),)
                .unwrap()
                .get(),
            4096
        );
        let mut profile = profile(json!({
            "thinking": {"type": "enabled", "budget_tokens": 5000}
        }));
        profile.runtime_max_output_tokens = 8192;
        let error = profile
            .validate_for_use_case(
                WireProtocol::AnthropicMessages,
                ModelUseCase::ContextCompression,
                Some(4096),
            )
            .unwrap_err();
        assert!(error.to_string().contains("effective output limit (4096)"));
    }

    #[test]
    fn applies_openai_options_and_one_output_field() {
        let profile = profile(json!({
            "thinking": {"type": "disabled"},
            "reasoning_effort": "high",
            "enable_thinking": false,
            "thinking_budget": 1024
        }));
        let mut body = json!({"model": "custom", "messages": [], "stream": true});
        apply_model_request_profile(
            WireProtocol::OpenAiChatCompletions,
            ModelUseCase::Agent,
            &profile,
            PositiveOutputLimit::new(4096).unwrap(),
            &mut body,
        )
        .unwrap();
        assert_eq!(body["max_tokens"], 4096);
        assert_eq!(body["thinking"]["type"], "disabled");
        assert!(body.get("max_completion_tokens").is_none());
    }

    #[test]
    fn applies_anthropic_adaptive_effort_at_top_level() {
        let profile = profile(json!({
            "thinking": {"type": "adaptive", "display": "omitted"},
            "output_config": {"effort": "high"}
        }));
        let mut body = json!({"model": "claude", "messages": [], "stream": true});
        apply_model_request_profile(
            WireProtocol::AnthropicMessages,
            ModelUseCase::Agent,
            &profile,
            PositiveOutputLimit::new(4096).unwrap(),
            &mut body,
        )
        .unwrap();
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["output_config"]["effort"], "high");
        assert!(body["thinking"].get("output_config").is_none());
    }

    #[test]
    fn rejects_manual_thinking_at_or_above_the_runtime_budget() {
        let profile = profile(json!({
            "thinking": {"type": "enabled", "budget_tokens": 4096}
        }));
        assert!(profile.validate(WireProtocol::AnthropicMessages).is_err());
    }

    #[test]
    fn probes_and_runtime_requests_use_the_same_output_budget_and_options() {
        for (protocol, field, options) in [
            (
                WireProtocol::OpenAiChatCompletions,
                OutputLimitField::MaxTokens,
                json!({"reasoning_effort": "high"}),
            ),
            (
                WireProtocol::OpenAiChatCompletions,
                OutputLimitField::MaxCompletionTokens,
                json!({}),
            ),
            (
                WireProtocol::AnthropicMessages,
                OutputLimitField::MaxTokens,
                json!({"thinking": {"type": "enabled", "budget_tokens": 2048}}),
            ),
        ] {
            let mut profile = profile(options);
            profile.output_limit_field = field;
            let mut bodies = Vec::new();
            for use_case in [
                ModelUseCase::Probe,
                ModelUseCase::Agent,
                ModelUseCase::Approval,
                ModelUseCase::Safety,
            ] {
                let effective = profile
                    .validate_for_use_case(protocol, use_case, None)
                    .unwrap();
                assert_eq!(effective.get(), profile.runtime_max_output_tokens);
                let mut body = json!({"model": "synthetic", "messages": []});
                apply_model_request_profile(protocol, use_case, &profile, effective, &mut body)
                    .unwrap();
                assert_eq!(body[field.as_str()], profile.runtime_max_output_tokens);
                bodies.push(body);
            }
            assert!(bodies.windows(2).all(|pair| pair[0] == pair[1]));
            assert!(
                serde_json::to_value(&profile)
                    .unwrap()
                    .get("probe_max_output_tokens")
                    .is_none()
            );
        }
    }

    #[test]
    fn rejects_unknown_and_reserved_options() {
        let unknown = profile(json!({"vendor_magic": true}));
        assert!(
            unknown
                .validate(WireProtocol::OpenAiChatCompletions)
                .is_err()
        );

        let reserved = profile(json!({"temperature": 0}));
        assert_eq!(
            reserved
                .validate(WireProtocol::OpenAiChatCompletions)
                .unwrap_err(),
            ProfileError::ReservedRequestOption("temperature")
        );
    }

    #[test]
    fn validates_context_application_bounds() {
        let mut candidate = profile(json!({}));
        candidate.max_context_bytes = (MIN_MODEL_CONTEXT_BYTES - 1) as i64;
        assert!(
            candidate
                .validate(WireProtocol::OpenAiChatCompletions)
                .is_err()
        );
        candidate.max_context_bytes = MAX_MODEL_CONTEXT_BYTES as i64 + 1;
        assert!(
            candidate
                .validate(WireProtocol::OpenAiChatCompletions)
                .is_err()
        );
    }

    #[test]
    fn native_clear_requires_one_explicit_valid_keep_policy() {
        for keep in [json!("all"), json!({"type":"thinking_turns","value":2})] {
            let candidate = profile(
                json!({"context_management":{"edits":[{"type":"clear_thinking_20251015","keep":keep}]}}),
            );
            assert!(candidate.validate(WireProtocol::AnthropicMessages).is_ok());
        }
        for options in [
            json!({"context_management":{"edits":[]}}),
            json!({"context_management":{"edits":[{"type":"clear_thinking_20251015"}]}}),
            json!({"context_management":{"edits":[{"type":"clear_thinking_20251015","keep":{"type":"thinking_turns","value":0}}]}}),
            json!({"context_management":{"edits":[{"type":"clear_tool_uses_20250919","keep":"all"}]}}),
            json!({"context_management":{"edits":[{"type":"clear_thinking_20251015","keep":"all","extra":true}]}}),
        ] {
            assert!(
                profile(options)
                    .validate(WireProtocol::AnthropicMessages)
                    .is_err()
            );
        }
    }
    #[test]
    fn reasoning_effort_and_prefix_are_explicit_contract_capabilities() {
        let mut candidate = profile(json!({"reasoning_effort":"max"}));
        assert!(
            candidate
                .validate(WireProtocol::OpenAiChatCompletions)
                .is_err()
        );
        candidate.reasoning_contract = ReasoningContract::DeepseekChat;
        assert!(
            candidate
                .validate(WireProtocol::OpenAiChatCompletions)
                .is_ok()
        );
        candidate.request_options = json!({"reasoning_effort":"medium"});
        assert!(
            candidate
                .validate(WireProtocol::OpenAiChatCompletions)
                .is_err()
        );
        candidate.request_options = json!({});
        candidate.anthropic_prefix_binding = true;
        assert!(
            candidate
                .validate(WireProtocol::OpenAiChatCompletions)
                .is_err()
        );
        candidate.reasoning_contract = ReasoningContract::AnthropicMessages;
        assert!(candidate.validate(WireProtocol::AnthropicMessages).is_ok());
    }
}
