use std::time::Instant;

use actix_web::{HttpResponse, get, post, web};
use desk_agent_protocol::provenance::AiProvenance;
use desk_diagnose_core::model_capability::ModelCapabilities;
use desk_diagnose_core::prompt::ResponseFormatSpec;
use desk_diagnose_core::provider_probe::{provider_probe_request, verify_probe_response};
use desk_diagnose_core::seam::{ModelRequest, ModelSeam, NullTurnSink};
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
    pub provider: String,
    pub model: String,
    pub supports_image_input: bool,
    pub base_url: String,
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
        provider: Some(params.provider),
        model: Some(params.model),
        supports_image_input: Some(params.supports_image_input),
        base_url: Some(params.base_url),
        api_key: params.api_key,
        ..Default::default()
    });
    candidate
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
    model_provider::save(db, config.clone()).await?;
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
    request.max_output_tokens = Some(expectation.max_output_tokens);

    let started = Instant::now();
    let mut sink = NullTurnSink;
    let turn = seam.call(request, &mut sink).await.map_err(|e| {
        DeskSignalError::new_custom_error(
            DeskErrorCode::SYSTEM_ERROR,
            &format!("Test failed: {}", e.message),
        )
    })?;
    let latency_ms = started.elapsed().as_millis() as u64;
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
    // Fail closed with a precondition error when the provider is not fully
    // configured (missing model / base_url / api_key).
    let seam = SignalModelSeam::from_config(&config).map_err(|e| {
        DeskSignalError::new_custom_error(DeskErrorCode::PRECONDITION_FAILED, &e.message)
    })?;
    // A reachable-but-broken chain (bad key, wrong model, unreachable host) is a
    // business outcome surfaced as the failure body with the real reason.
    let dto = run_probe(&seam, config.model.clone(), config.supports_image_input).await?;
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
            assert_eq!(
                request.max_output_tokens,
                Some(if visual { 64 } else { 16 })
            );
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
            provider: Some("openai-compatible".into()),
            model: Some("saved-model".into()),
            supports_image_input: false,
            base_url: Some("https://saved.example/v1".into()),
            api_key: Some("saved-key".into()),
            ..Default::default()
        };

        let candidate = config_for_probe(
            &stored,
            ProviderTestParams {
                provider: "anthropic".into(),
                model: "unsaved-model".into(),
                supports_image_input: true,
                base_url: "https://unsaved.example".into(),
                api_key: Some("unsaved-key".into()),
            },
        );

        assert_eq!(candidate.provider.as_deref(), Some("anthropic"));
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
                provider: "openai-compatible".into(),
                model: "model".into(),
                supports_image_input: false,
                base_url: "https://example.com/v1".into(),
                api_key: None,
            },
        );

        assert_eq!(candidate.api_key.as_deref(), Some("saved-key"));
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
