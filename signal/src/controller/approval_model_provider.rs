//! Local-owner configuration and behavioral probe for the independent OSS
//! permission reviewer. The secret is write-only; no device tool is available
//! during a review or its probe.

use std::time::Instant;

use actix_web::{HttpResponse, get, post, web};
use desk_diagnose_core::approval_review::{
    APPROVAL_REVIEW_SYSTEM_PROMPT, approval_probe_cases, parse_review_decision, review_user_prompt,
};
use desk_diagnose_core::chat::{ChatMessage, ChatRole, StopReason};
use desk_diagnose_core::model_profile::{ModelUseCase, OutputLimitField, WireProtocol};
use desk_diagnose_core::prompt::ResponseFormatSpec;
use desk_diagnose_core::seam::{ModelRequest, ModelSeam, NullTurnSink};
use desk_utils::error::DeskErrorCode;
use desk_utils::rest::RestResponse;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::approval_model_provider::{self, ApprovalModelPublic, ApprovalModelUpdate};
use crate::error::DeskSignalError;
use crate::model_dial::{SignalModelSeam, configured_enforce_public_tls, configured_ssrf_mode};
use crate::model_provider::ModelProbeObservation;

pub const TAG: &str = "ApprovalModelProvider";

#[derive(Deserialize, ToSchema)]
pub struct ApprovalModelProbeParams {
    #[schema(value_type = String)]
    pub wire_protocol: WireProtocol,
    pub model: String,
    pub base_url: String,
    pub api_key: Option<String>,
    #[schema(value_type = Object)]
    pub request_options: serde_json::Value,
    #[schema(value_type = String)]
    pub output_limit_field: OutputLimitField,
    pub probe_max_output_tokens: i64,
    pub runtime_max_output_tokens: i64,
    pub max_context_bytes: i64,
}

#[derive(Serialize, ToSchema)]
pub struct ApprovalModelProbeDto {
    pub latency_ms: u64,
    pub validated_capabilities: Vec<String>,
    pub reasoning_observed: bool,
    pub reasoning_tokens: Option<i64>,
    pub saved_as_current: bool,
}

fn reject_invalid(error: impl std::fmt::Display) -> DeskSignalError {
    DeskSignalError::new_custom_error(DeskErrorCode::INVALID_PARAMS, &error.to_string())
}

fn validate_provider_url(
    config: &approval_model_provider::ApprovalModelConfig,
) -> Result<(), DeskSignalError> {
    if let Some(base_url) = config.gateway.base_url.as_deref()
        && !base_url.trim().is_empty()
    {
        desk_utils::ssrf::check_provider_url(
            base_url,
            configured_ssrf_mode(),
            configured_enforce_public_tls(),
        )
        .map_err(|_| reject_invalid("approval model base_url is not permitted"))?;
    }
    Ok(())
}

#[utoipa::path(
    tag = TAG,
    summary = "Query masked independent approval model configuration",
    responses((status = 200, body = RestResponse<ApprovalModelPublic>)),
)]
#[get("/approval-provider")]
pub async fn get_approval_model_provider() -> Result<HttpResponse, DeskSignalError> {
    let config = approval_model_provider::load(crate::db::get_db()).await?;
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(config.public_view())))
}

#[utoipa::path(
    tag = TAG,
    summary = "Update independent approval model configuration",
    request_body = ApprovalModelUpdate,
    responses((status = 200, body = RestResponse<ApprovalModelPublic>)),
)]
#[post("/approval-provider")]
pub async fn update_approval_model_provider(
    body: web::Json<ApprovalModelUpdate>,
) -> Result<HttpResponse, DeskSignalError> {
    let db = crate::db::get_db();
    let mut config = approval_model_provider::load(db).await?;
    let update = body.into_inner();
    if update
        .prices
        .is_some_and(|prices| prices.validate().is_none())
    {
        return Err(reject_invalid(
            "approval model token prices must include positive input and output rates",
        ));
    }
    let expected = (
        update.expected_configuration_revision,
        update.expected_connection_revision,
        update.expected_profile_revision,
    );
    if expected
        != (
            config.configuration_revision,
            config.gateway.connection_revision,
            config.gateway.profile_revision,
        )
    {
        return Err(DeskSignalError::new_custom_error(
            DeskErrorCode::PRECONDITION_FAILED,
            "approval model configuration revision conflict",
        ));
    }
    config.apply_update(update);
    config.gateway.request_profile().map_err(reject_invalid)?;
    validate_provider_url(&config)?;
    if !approval_model_provider::save_if_revisions_match(
        db,
        config.clone(),
        expected.0,
        expected.1,
        expected.2,
    )
    .await?
    {
        return Err(DeskSignalError::new_custom_error(
            DeskErrorCode::PRECONDITION_FAILED,
            "approval model configuration revision conflict",
        ));
    }
    Ok(HttpResponse::Ok().json(RestResponse::succeed_with_data(config.public_view())))
}

#[utoipa::path(
    tag = TAG,
    summary = "Probe independent approval decisions without device tools",
    request_body = ApprovalModelProbeParams,
    responses((status = 200, body = RestResponse<ApprovalModelProbeDto>)),
)]
#[post("/approval-provider/test")]
pub async fn test_approval_model_provider(
    body: web::Json<ApprovalModelProbeParams>,
) -> Result<HttpResponse, DeskSignalError> {
    let db = crate::db::get_db();
    let stored = approval_model_provider::load(db).await?;
    let mut candidate = stored.clone();
    let params = body.into_inner();
    candidate.apply_update(ApprovalModelUpdate {
        expected_configuration_revision: stored.configuration_revision,
        expected_connection_revision: stored.gateway.connection_revision,
        expected_profile_revision: stored.gateway.profile_revision,
        wire_protocol: Some(params.wire_protocol),
        model: Some(params.model),
        base_url: Some(params.base_url),
        api_key: params.api_key,
        request_options: Some(params.request_options),
        output_limit_field: Some(params.output_limit_field),
        probe_max_output_tokens: Some(params.probe_max_output_tokens),
        runtime_max_output_tokens: Some(params.runtime_max_output_tokens),
        max_context_bytes: Some(params.max_context_bytes),
        ..Default::default()
    });
    candidate
        .gateway
        .request_profile()
        .map_err(reject_invalid)?;
    validate_provider_url(&candidate)?;
    let seam = SignalModelSeam::from_approval_config(&candidate).map_err(|error| {
        DeskSignalError::new_custom_error(DeskErrorCode::PRECONDITION_FAILED, &error.message)
    })?;
    let started = Instant::now();
    let mut capabilities = Vec::new();
    let mut reasoning_observed = false;
    let mut reasoning_tokens = 0_u64;
    for probe in approval_probe_cases() {
        let prompt = review_user_prompt(&probe.candidate)
            .map_err(|error| reject_invalid(format!("invalid approval probe: {error:?}")))?;
        let mut request = ModelRequest::text_only(
            vec![
                ChatMessage::text(
                    format!("{}-system", probe.candidate.candidate_id),
                    ChatRole::System,
                    APPROVAL_REVIEW_SYSTEM_PROMPT.to_owned(),
                ),
                ChatMessage::text(
                    format!("{}-user", probe.candidate.candidate_id),
                    ChatRole::User,
                    prompt,
                ),
            ],
            ResponseFormatSpec::JsonObject,
        );
        request.use_case = ModelUseCase::Probe;
        let mut sink = NullTurnSink;
        let turn = seam.call(request, &mut sink).await.map_err(|error| {
            DeskSignalError::new_custom_error(DeskErrorCode::SYSTEM_ERROR, &error.message)
        })?;
        if turn.stop_reason != StopReason::EndTurn || !turn.tool_calls.is_empty() {
            return Err(DeskSignalError::new_custom_error(
                DeskErrorCode::SYSTEM_ERROR,
                "approval model probe did not finish with one text decision",
            ));
        }
        let decision = parse_review_decision(&probe.candidate, &turn.text).map_err(|error| {
            DeskSignalError::new_custom_error(
                DeskErrorCode::SYSTEM_ERROR,
                &format!("approval model returned an invalid probe decision: {error:?}"),
            )
        })?;
        if decision.verdict != probe.expected_verdict {
            return Err(DeskSignalError::new_custom_error(
                DeskErrorCode::SYSTEM_ERROR,
                "approval model returned the wrong probe verdict",
            ));
        }
        capabilities.push(probe.key.to_owned());
        reasoning_observed |= turn.provider_meta.reasoning_observed;
        reasoning_tokens =
            reasoning_tokens.saturating_add(turn.provider_meta.reasoning_tokens.unwrap_or(0));
    }
    let observation = ModelProbeObservation {
        connection_revision: candidate.gateway.connection_revision,
        profile_revision: candidate.gateway.profile_revision,
        tested_at: chrono::Utc::now(),
        reasoning_observed,
        reasoning_tokens: reasoning_observed
            .then(|| i64::try_from(reasoning_tokens).ok())
            .flatten(),
        stop_reason: Some("endturn".into()),
        validated_capabilities: serde_json::Value::Object(
            capabilities
                .iter()
                .map(|name| (name.clone(), serde_json::Value::Bool(true)))
                .collect(),
        ),
        current: true,
    };
    let saved_as_current =
        approval_model_provider::save_probe_if_current(db, &candidate, observation).await?;
    Ok(
        HttpResponse::Ok().json(RestResponse::succeed_with_data(ApprovalModelProbeDto {
            latency_ms: started.elapsed().as_millis() as u64,
            validated_capabilities: capabilities,
            reasoning_observed,
            reasoning_tokens: reasoning_observed
                .then(|| i64::try_from(reasoning_tokens).ok())
                .flatten(),
            saved_as_current,
        })),
    )
}
