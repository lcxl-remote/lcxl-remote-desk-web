use std::time::Instant;

use actix_web::{HttpResponse, get, post, web};
use desk_agent_protocol::provenance::AiProvenance;
use desk_diagnose_core::chat::{ChatMessage, ChatRole};
use desk_diagnose_core::prompt::ResponseFormatSpec;
use desk_diagnose_core::seam::{ModelRequest, ModelSeam, NullTurnSink};
use desk_utils::error::DeskErrorCode;
use desk_utils::rest::RestResponse;
use serde::Serialize;
use utoipa::ToSchema;

use crate::error::DeskSignalError;
use crate::model_dial::{SignalModelSeam, configured_enforce_public_tls, configured_ssrf_mode};
use crate::model_provider::{self, ModelProviderPublic, ModelProviderUpdate};

pub const TAG: &str = "ModelProvider";

/// Small output cap for the connectivity probe. The reply is a single word, so a
/// tiny ceiling keeps a misconfigured model from streaming a wall of text.
const PROBE_MAX_OUTPUT_TOKENS: u32 = 16;

/// Result of a successful provider connectivity test. The `api_key` stays
/// server-side; only latency and a bounded reply snippet are returned.
#[derive(Debug, Serialize, ToSchema)]
pub struct ProviderTestDto {
    /// Round-trip latency of the probe call, in milliseconds.
    pub latency_ms: u64,
    /// A short snippet of the model's reply (bounded), when it returned text.
    pub sample: Option<String>,
    /// Machine-readable AI marking for `sample` (EU AI Act Art.50(2)): the snippet
    /// is model-generated text shown to the operator, so it carries a marking.
    /// Present only when a `sample` is returned; absent otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provenance: Option<AiProvenance>,
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
    config.apply_update(body.into_inner());
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
) -> Result<ProviderTestDto, DeskSignalError> {
    let mut request = ModelRequest::text_only(
        vec![ChatMessage::text(
            "probe",
            ChatRole::User,
            "Reply with the single word: pong",
        )],
        ResponseFormatSpec::None,
    );
    // A one-word reply — cap output so a misconfigured model cannot stream forever.
    request.max_output_tokens = Some(PROBE_MAX_OUTPUT_TOKENS);

    let started = Instant::now();
    let mut sink = NullTurnSink;
    let turn = seam.call(request, &mut sink).await.map_err(|e| {
        DeskSignalError::new_custom_error(
            DeskErrorCode::SYSTEM_ERROR,
            &format!("Test failed: {}", e.message),
        )
    })?;
    let latency_ms = started.elapsed().as_millis() as u64;
    // Bound the snippet; do not match the text exactly (a small cap may truncate a
    // healthy reply), just prove the chain returned something.
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
        provenance,
    })
}

#[utoipa::path(
    tag = TAG,
    summary = "Test the model-provider AI call chain",
    responses(
        (status = 200, description = "Connectivity test result", body = RestResponse<ProviderTestDto>),
    ),
)]
#[post("/provider/test")]
pub async fn test_model_provider() -> Result<HttpResponse, DeskSignalError> {
    let db = crate::db::get_db();
    let config = model_provider::load(db).await?;
    // Fail closed with a precondition error when the provider is not fully
    // configured (missing model / base_url / api_key).
    let seam = SignalModelSeam::from_config(&config).map_err(|e| {
        DeskSignalError::new_custom_error(DeskErrorCode::PRECONDITION_FAILED, &e.message)
    })?;
    // A reachable-but-broken chain (bad key, wrong model, unreachable host) is a
    // business outcome surfaced as the failure body with the real reason.
    let dto = run_probe(&seam, config.model.clone()).await?;
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
            assert_eq!(
                request.max_output_tokens,
                Some(PROBE_MAX_OUTPUT_TOKENS),
                "probe must cap the output tokens"
            );
            Ok(ModelTurn {
                text: "  pong  ".into(),
                ..Default::default()
            })
        }
    }

    /// A seam that always fails, standing in for an unreachable / misconfigured
    /// upstream.
    struct ErrSeam;

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
        let dto = run_probe(&OkSeam, Some("gpt-4o".into()))
            .await
            .expect("probe ok");
        assert_eq!(dto.sample.as_deref(), Some("pong"));
    }

    /// A returned sample is marked AI-generated (Art.50(2)) with the probed model.
    #[tokio::test]
    async fn probe_sample_is_marked_with_the_model() {
        let dto = run_probe(&OkSeam, Some("gpt-4o".into()))
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
        let err = run_probe(&ErrSeam, Some("gpt-4o".into()))
            .await
            .expect_err("probe should fail");
        assert!(matches!(err, DeskSignalError::CustomError(_)));
        assert!(
            err.to_string().contains("boom"),
            "the upstream reason should pass through: {err}"
        );
    }
}
