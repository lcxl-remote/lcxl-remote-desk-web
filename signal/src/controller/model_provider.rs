use std::time::Instant;

use actix_web::{HttpResponse, get, post, web};
use desk_agent_protocol::provenance::AiProvenance;
use desk_diagnose_core::model_capability::ModelCapabilities;
use desk_diagnose_core::model_profile::{ModelUseCase, OutputLimitField, WireProtocol};
use desk_diagnose_core::prompt::ResponseFormatSpec;
use desk_diagnose_core::provider_probe::{provider_probe_request, verify_probe_response};
use desk_diagnose_core::seam::{ModelRequest, ModelSeam, NullTurnSink};
use desk_diagnose_core::terminal_complete::COMPLETION_MAX_OUTPUT_TOKENS;
use desk_utils::error::DeskErrorCode;
use desk_utils::rest::RestResponse;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::error::DeskSignalError;
use crate::model_dial::{SignalModelSeam, configured_enforce_public_tls, configured_ssrf_mode};
use crate::model_provider::{self, ModelProviderPublic, ModelProviderUpdate};

pub const TAG: &str = "ModelProvider";

/// Result of a successful provider connectivity test. The `api_key` stays
/// server-side; only latency and a bounded reply snippet are returned.
#[derive(Debug, Serialize, ToSchema)]
pub struct ProviderTestDto {
    /// Round-trip latency of the probe call, in milliseconds.
    pub latency_ms: u64,
    /// A short snippet of the model's reply (bounded), when it returned text.
    pub sample: Option<String>,
    /// Capabilities that this exact probe exercised successfully.
    pub validated_capabilities: Vec<String>,
    pub reasoning_observed: bool,
    pub reasoning_tokens: Option<i64>,
    pub stop_reason: String,
    /// Machine-readable AI marking for `sample` (EU AI Act Art.50(2)): the snippet
    /// is model-generated text shown to the operator, so it carries a marking.
    /// Present only when a `sample` is returned; absent otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provenance: Option<AiProvenance>,
}

/// Unsaved connection fields used for one provider probe. An omitted API key
/// reuses the stored secret; a supplied key is kept in memory for this request.
#[derive(Deserialize, ToSchema)]
pub struct ProviderTestParams {
    #[schema(value_type = String)]
    pub wire_protocol: WireProtocol,
    pub model: String,
    pub supports_image_input: bool,
    pub base_url: String,
    #[schema(value_type = Object)]
    pub request_options: serde_json::Value,
    #[schema(value_type = String)]
    pub output_limit_field: OutputLimitField,
    pub probe_max_output_tokens: i64,
    pub runtime_max_output_tokens: i64,
    #[schema(minimum = 4096, maximum = 16777216)]
    pub max_context_bytes: i64,
    /// Write-only. `None` reuses the stored key; an empty string clears it for
    /// this probe; any other value is used only for this probe.
    pub api_key: Option<String>,
}

fn config_for_probe(
    stored: &model_provider::ModelProviderConfig,
    params: ProviderTestParams,
) -> model_provider::ModelProviderConfig {
    let mut candidate = stored.clone();
    candidate.apply_update(ModelProviderUpdate {
        wire_protocol: Some(params.wire_protocol),
        model: Some(params.model),
        supports_image_input: Some(params.supports_image_input),
        base_url: Some(params.base_url),
        request_options: Some(params.request_options),
        output_limit_field: Some(params.output_limit_field),
        probe_max_output_tokens: Some(params.probe_max_output_tokens),
        runtime_max_output_tokens: Some(params.runtime_max_output_tokens),
        max_context_bytes: Some(params.max_context_bytes),
        api_key: params.api_key,
        ..Default::default()
    });
    candidate
}

/// The OSS singleton serves both the conversational agent and terminal
/// completion. Validate the stored/candidate profile against the completion
/// caller's smaller hard cap so a successful save or probe cannot create a
/// configuration that deterministically fails every completion request.
fn validate_shared_profile(
    config: &model_provider::ModelProviderConfig,
) -> Result<(), DeskSignalError> {
    let profile = config.request_profile().map_err(|error| {
        DeskSignalError::new_custom_error(DeskErrorCode::INVALID_PARAMS, &error.to_string())
    })?;
    let protocol = config.wire_protocol.ok_or_else(|| {
        DeskSignalError::new_custom_error(
            DeskErrorCode::INVALID_PARAMS,
            "wire protocol is required",
        )
    })?;
    profile
        .validate_for_use_case(
            protocol,
            ModelUseCase::Completion,
            Some(i64::from(COMPLETION_MAX_OUTPUT_TOKENS)),
        )
        .map_err(|error| {
            DeskSignalError::new_custom_error(DeskErrorCode::INVALID_PARAMS, &error.to_string())
        })?;
    Ok(())
}

#[utoipa::path(
    tag = TAG,
    summary = "Query the masked model-provider configuration",
    responses(
        (status = 200, description = "Masked provider config (never carries the api_key)", body = RestResponse<ModelProviderPublic>),
    ),
)]
#[get("/provider")]
pub async fn get_model_provider() -> Result<HttpResponse, DeskSignalError> {
    let db = crate::db::get_db();
    let config = model_provider::load(db).await?;
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(config.public_view())))
}

#[utoipa::path(
    tag = TAG,
    summary = "Update the model-provider configuration",
    request_body = ModelProviderUpdate,
    responses(
        (status = 200, description = "Updated masked provider config", body = RestResponse<ModelProviderPublic>),
    ),
)]
#[post("/provider")]
pub async fn update_model_provider(
    body: web::Json<ModelProviderUpdate>,
) -> Result<HttpResponse, DeskSignalError> {
    let db = crate::db::get_db();
    let mut config = model_provider::load(db).await?;
    let update = body.into_inner();
    let expected_connection_revision = update.expected_connection_revision.ok_or_else(|| {
        DeskSignalError::new_custom_error(
            DeskErrorCode::PRECONDITION_FAILED,
            "provider connection revision is required",
        )
    })?;
    let expected_profile_revision = update.expected_profile_revision.ok_or_else(|| {
        DeskSignalError::new_custom_error(
            DeskErrorCode::PRECONDITION_FAILED,
            "provider profile revision is required",
        )
    })?;
    if expected_connection_revision != config.connection_revision
        || expected_profile_revision != config.profile_revision
    {
        return Err(DeskSignalError::new_custom_error(
            DeskErrorCode::PRECONDITION_FAILED,
            "provider configuration revision conflict",
        ));
    }
    if let Some(limit) = update.max_steps_per_turn
        && !(model_provider::MAX_STEPS_MIN..=model_provider::MAX_STEPS_MAX).contains(&limit)
    {
        return Err(DeskSignalError::new_custom_error(
            DeskErrorCode::INVALID_PARAMS,
            &format!(
                "max_steps_per_turn must be between {} and {}",
                model_provider::MAX_STEPS_MIN,
                model_provider::MAX_STEPS_MAX
            ),
        ));
    }
    if let Some(limit) = update.max_same_tool_calls_per_turn
        && !(model_provider::MAX_SAME_TOOL_CALLS_MIN..=model_provider::MAX_SAME_TOOL_CALLS_MAX)
            .contains(&limit)
    {
        return Err(DeskSignalError::new_custom_error(
            DeskErrorCode::INVALID_PARAMS,
            &format!(
                "max_same_tool_calls_per_turn must be between {} and {}",
                model_provider::MAX_SAME_TOOL_CALLS_MIN,
                model_provider::MAX_SAME_TOOL_CALLS_MAX
            ),
        ));
    }
    if let Some(timeout) = update.exec_approval_timeout_secs
        && !(model_provider::EXEC_APPROVAL_TIMEOUT_MIN_SECS
            ..=model_provider::EXEC_APPROVAL_TIMEOUT_MAX_SECS)
            .contains(&timeout)
    {
        return Err(DeskSignalError::new_custom_error(
            DeskErrorCode::INVALID_PARAMS,
            &format!(
                "exec_approval_timeout_secs must be between {} and {}",
                model_provider::EXEC_APPROVAL_TIMEOUT_MIN_SECS,
                model_provider::EXEC_APPROVAL_TIMEOUT_MAX_SECS
            ),
        ));
    }
    let next_max_steps = update
        .max_steps_per_turn
        .unwrap_or(config.max_steps_per_turn);
    let next_same_tool_limit = update
        .max_same_tool_calls_per_turn
        .unwrap_or(config.max_same_tool_calls_per_turn);
    if !model_provider::step_budget_covers_same_tool_limit(next_max_steps, next_same_tool_limit) {
        return Err(DeskSignalError::new_custom_error(
            DeskErrorCode::INVALID_PARAMS,
            "max_steps_per_turn must be greater than or equal to \
             max_same_tool_calls_per_turn",
        ));
    }
    config.apply_update(update);
    validate_shared_profile(&config)?;
    // Write-time SSRF check: reject a base_url whose scheme or IP-literal host is
    // not permitted by the active mode. Domain hosts pass here and are re-checked
    // authoritatively at dial time by the connect-time resolver. An unset base_url
    // is allowed (the seam fails closed at dial time when it is missing).
    if let Some(base_url) = config.base_url.as_deref()
        && !base_url.trim().is_empty()
    {
        desk_utils::ssrf::check_provider_url(
            base_url,
            configured_ssrf_mode(),
            configured_enforce_public_tls(),
        )
        .map_err(|_| {
            DeskSignalError::new_custom_error(
                DeskErrorCode::INVALID_PARAMS,
                "base_url is not permitted by the server's provider policy",
            )
        })?;
    }
    if !model_provider::save_if_revisions_match(
        db,
        config.clone(),
        expected_connection_revision,
        expected_profile_revision,
    )
    .await?
    {
        return Err(DeskSignalError::new_custom_error(
            DeskErrorCode::PRECONDITION_FAILED,
            "provider configuration revision conflict",
        ));
    }
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(config.public_view())))
}

/// Run a minimal chat probe against `seam` and shape the outcome. Kept separate
/// from the handler so tests can inject a stub [`ModelSeam`] and exercise the
/// success / error mapping without a real upstream call.
async fn run_probe(
    seam: &dyn ModelSeam,
    model: Option<String>,
    supports_image_input: bool,
) -> Result<ProviderTestDto, DeskSignalError> {
    let expectation = provider_probe_request(ModelCapabilities {
        image_input: supports_image_input,
    });
    let mut request =
        ModelRequest::text_only(vec![expectation.message.clone()], ResponseFormatSpec::None);
    request.use_case = ModelUseCase::Probe;

    let started = Instant::now();
    let mut sink = NullTurnSink;
    let turn = seam.call(request, &mut sink).await.map_err(|e| {
        DeskSignalError::new_custom_error(
            DeskErrorCode::SYSTEM_ERROR,
            &format!("Test failed: {}", e.message),
        )
    })?;
    let latency_ms = started.elapsed().as_millis() as u64;
    if turn.provider_meta.reasoning_observed
        && turn.stop_reason == desk_diagnose_core::chat::StopReason::MaxTokens
        && turn.text.trim().is_empty()
    {
        return Err(DeskSignalError::new_custom_error(
            DeskErrorCode::PRECONDITION_FAILED,
            "Test failed: the reasoning budget exhausted the probe output limit before any answer was produced; increase probe_max_output_tokens",
        ));
    }
    verify_probe_response(&expectation, &turn.text).map_err(|message| {
        DeskSignalError::new_custom_error(
            if expectation.required_marker.is_some() {
                DeskErrorCode::AI_MODEL_IMAGE_INPUT_UNSUPPORTED
            } else {
                DeskErrorCode::SYSTEM_ERROR
            },
            &format!("Test failed: {message}"),
        )
    })?;
    let trimmed = turn.text.trim();
    let sample: Option<String> = if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.chars().take(200).collect())
    };
    // Mark the AI-generated snippet (Art.50(2)) only when the probe returned text.
    let provenance = sample
        .is_some()
        .then(|| AiProvenance::stamp(model, Some(chrono::Utc::now().to_rfc3339())));
    Ok(ProviderTestDto {
        latency_ms,
        sample,
        validated_capabilities: expectation.validated_capabilities(),
        reasoning_observed: turn.provider_meta.reasoning_observed,
        reasoning_tokens: turn
            .provider_meta
            .reasoning_tokens
            .and_then(|tokens| i64::try_from(tokens).ok()),
        stop_reason: serde_json::to_value(turn.stop_reason)
            .ok()
            .and_then(|value| value.as_str().map(str::to_string))
            .unwrap_or_else(|| "other".to_string()),
        provenance,
    })
}

#[utoipa::path(
    tag = TAG,
    summary = "Test the model-provider AI call chain",
    request_body = ProviderTestParams,
    responses(
        (status = 200, description = "Connectivity test result", body = RestResponse<ProviderTestDto>),
    ),
)]
#[post("/provider/test")]
pub async fn test_model_provider(
    body: web::Json<ProviderTestParams>,
) -> Result<HttpResponse, DeskSignalError> {
    let db = crate::db::get_db();
    let stored = model_provider::load(db).await?;
    // Overlay the form values in memory. The candidate is deliberately never
    // passed to `save`, so testing cannot commit an unverified configuration.
    let config = config_for_probe(&stored, body.into_inner());
    validate_shared_profile(&config)?;
    // Fail closed with a precondition error when the provider is not fully
    // configured (missing model / base_url / api_key).
    let seam = SignalModelSeam::from_config(&config).map_err(|e| {
        DeskSignalError::new_custom_error(DeskErrorCode::PRECONDITION_FAILED, &e.message)
    })?;
    // A reachable-but-broken chain (bad key, wrong model, unreachable host) is a
    // business outcome surfaced as the failure body with the real reason.
    let dto = run_probe(&seam, config.model.clone(), config.supports_image_input).await?;
    let validated_capabilities = serde_json::Value::Object(
        dto.validated_capabilities
            .iter()
            .map(|capability| (capability.clone(), serde_json::Value::Bool(true)))
            .collect(),
    );
    let _stored = model_provider::save_probe_observation_if_current(
        db,
        model_provider::ModelProbeObservation {
            connection_revision: config.connection_revision,
            profile_revision: config.profile_revision,
            tested_at: chrono::Utc::now(),
            reasoning_observed: dto.reasoning_observed,
            reasoning_tokens: dto.reasoning_tokens,
            stop_reason: Some(dto.stop_reason.clone()),
            validated_capabilities,
            current: true,
        },
    )
    .await?;
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(dto)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::{AgentError, AgentErrorKind};
    use desk_diagnose_core::chat::ModelTurn;
    use desk_diagnose_core::seam::TurnSink;

    /// A seam that returns a fixed reply and asserts the probe capped its output.
    struct OkSeam;

    #[async_trait::async_trait(?Send)]
    impl ModelSeam for OkSeam {
        async fn call(
            &self,
            request: ModelRequest,
            _sink: &mut dyn TurnSink,
        ) -> Result<ModelTurn, AgentError> {
            let visual = request.messages[0].image_data_url.is_some();
            assert_eq!(request.use_case, ModelUseCase::Probe);
            assert_eq!(request.caller_output_hard_cap, None);
            Ok(ModelTurn {
                text: if visual { "LCXL7F" } else { "pong" }.into(),
                ..Default::default()
            })
        }
    }

    /// A seam that always fails, standing in for an unreachable / misconfigured
    /// upstream.
    struct ErrSeam;

    struct BlindSeam;

    #[test]
    fn probe_params_overlay_form_values_without_mutating_stored_config() {
        let stored = model_provider::ModelProviderConfig {
            wire_protocol: Some(WireProtocol::OpenAiChatCompletions),
            model: Some("saved-model".into()),
            supports_image_input: false,
            base_url: Some("https://saved.example/v1".into()),
            api_key: Some("saved-key".into()),
            ..Default::default()
        };

        let candidate = config_for_probe(
            &stored,
            ProviderTestParams {
                wire_protocol: WireProtocol::AnthropicMessages,
                model: "unsaved-model".into(),
                supports_image_input: true,
                base_url: "https://unsaved.example".into(),
                request_options: serde_json::json!({}),
                output_limit_field: OutputLimitField::MaxTokens,
                probe_max_output_tokens: 512,
                runtime_max_output_tokens: 4096,
                max_context_bytes: 131_072,
                api_key: Some("unsaved-key".into()),
            },
        );

        assert_eq!(
            candidate.wire_protocol,
            Some(WireProtocol::AnthropicMessages)
        );
        assert_eq!(candidate.model.as_deref(), Some("unsaved-model"));
        assert!(candidate.supports_image_input);
        assert_eq!(
            candidate.base_url.as_deref(),
            Some("https://unsaved.example")
        );
        assert_eq!(candidate.api_key.as_deref(), Some("unsaved-key"));
        assert_eq!(stored.model.as_deref(), Some("saved-model"));
        assert_eq!(stored.api_key.as_deref(), Some("saved-key"));
    }

    #[test]
    fn probe_without_a_typed_key_reuses_the_stored_secret() {
        let stored = model_provider::ModelProviderConfig {
            api_key: Some("saved-key".into()),
            ..Default::default()
        };
        let candidate = config_for_probe(
            &stored,
            ProviderTestParams {
                wire_protocol: WireProtocol::OpenAiChatCompletions,
                model: "model".into(),
                supports_image_input: false,
                base_url: "https://example.com/v1".into(),
                request_options: serde_json::json!({}),
                output_limit_field: OutputLimitField::MaxTokens,
                probe_max_output_tokens: 512,
                runtime_max_output_tokens: 4096,
                max_context_bytes: 131_072,
                api_key: None,
            },
        );

        assert_eq!(candidate.api_key.as_deref(), Some("saved-key"));
    }

    #[test]
    fn shared_oss_profile_rejects_manual_thinking_above_completion_cap() {
        let config = model_provider::ModelProviderConfig {
            wire_protocol: Some(WireProtocol::AnthropicMessages),
            model: Some("claude-custom".into()),
            base_url: Some("https://api.example".into()),
            api_key: Some("secret".into()),
            request_options: serde_json::json!({
                "thinking": {"type": "enabled", "budget_tokens": 600}
            }),
            output_limit_field: OutputLimitField::MaxTokens,
            probe_max_output_tokens: 1024,
            runtime_max_output_tokens: 4096,
            max_context_bytes: Some(131_072),
            ..Default::default()
        };

        let error = validate_shared_profile(&config)
            .expect_err("the singleton must remain valid for terminal completion");
        assert!(error.to_string().contains("effective output limit (512)"));
    }

    #[async_trait::async_trait(?Send)]
    impl ModelSeam for BlindSeam {
        async fn call(
            &self,
            _request: ModelRequest,
            _sink: &mut dyn TurnSink,
        ) -> Result<ModelTurn, AgentError> {
            Ok(ModelTurn {
                text: "I received your request".into(),
                ..Default::default()
            })
        }
    }

    #[async_trait::async_trait(?Send)]
    impl ModelSeam for ErrSeam {
        async fn call(
            &self,
            _request: ModelRequest,
            _sink: &mut dyn TurnSink,
        ) -> Result<ModelTurn, AgentError> {
            Err(AgentError {
                kind: AgentErrorKind::Internal,
                message: "boom".into(),
                retryable: false,
                safe_for_model: true,
                error_code: None,
            })
        }
    }

    #[tokio::test]
    async fn probe_success_trims_reply_into_sample() {
        let dto = run_probe(&OkSeam, Some("gpt-4o".into()), false)
            .await
            .expect("probe ok");
        assert_eq!(dto.sample.as_deref(), Some("pong"));
    }

    /// A returned sample is marked AI-generated (Art.50(2)) with the probed model.
    #[tokio::test]
    async fn probe_sample_is_marked_with_the_model() {
        let dto = run_probe(&OkSeam, Some("gpt-4o".into()), false)
            .await
            .expect("probe ok");
        let prov = dto.provenance.expect("a returned sample carries a marking");
        assert_eq!(prov.model_id.as_deref(), Some("gpt-4o"));
        assert_eq!(
            prov.marking_scheme.as_deref(),
            Some(desk_agent_protocol::provenance::AI_MARKING_SCHEME_V1)
        );
    }

    #[tokio::test]
    async fn probe_error_surfaces_upstream_reason_as_business_failure() {
        let err = run_probe(&ErrSeam, Some("gpt-4o".into()), false)
            .await
            .expect_err("probe should fail");
        assert!(matches!(err, DeskSignalError::CustomError(_)));
        assert!(
            err.to_string().contains("boom"),
            "the upstream reason should pass through: {err}"
        );
    }

    #[tokio::test]
    async fn visual_probe_proves_image_access() {
        let dto = run_probe(&OkSeam, Some("gpt-4o".into()), true)
            .await
            .expect("visual probe ok");
        assert_eq!(dto.validated_capabilities, vec!["text", "image_input"]);
        assert_eq!(dto.sample.as_deref(), Some("LCXL7F"));
    }

    #[tokio::test]
    async fn visual_probe_rejects_a_non_marker_response() {
        let error = run_probe(&BlindSeam, Some("gpt-4o".into()), true)
            .await
            .expect_err("a generic reply does not prove image access");
        assert!(error.to_string().contains("did not prove image access"));
    }
}
