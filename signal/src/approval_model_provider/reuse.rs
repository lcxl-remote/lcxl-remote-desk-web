use super::{ApprovalModelConfig, ApprovalModelUpdate};
use crate::{
    config::{ConfigConnection, connection::DatabaseConnection},
    error::DeskSignalError,
};
use desk_utils::error::DeskErrorCode;
use serde::Deserialize;
use utoipa::ToSchema;

#[derive(Clone, Copy, Deserialize, ToSchema)]
pub struct ApprovalModelReuseParams {
    pub expected_configuration_revision: i64,
    pub expected_connection_revision: i64,
    pub expected_profile_revision: i64,
}

pub(crate) fn validate_provider_url(config: &ApprovalModelConfig) -> Result<(), DeskSignalError> {
    if let Some(base_url) = config.gateway.base_url.as_deref()
        && !base_url.trim().is_empty()
    {
        desk_utils::ssrf::check_provider_url(
            base_url,
            crate::model_dial::configured_ssrf_mode(),
            crate::model_dial::configured_enforce_public_tls(),
        )
        .map_err(|_| {
            DeskSignalError::new_custom_error(
                DeskErrorCode::INVALID_PARAMS,
                "approval model base_url is not permitted",
            )
        })?;
    }
    Ok(())
}

/// Copy the latest saved gateway under the same guard that protects the target.
pub async fn reuse_ai_gateway(
    db: &DatabaseConnection,
    params: &ApprovalModelReuseParams,
) -> Result<ApprovalModelConfig, DeskSignalError> {
    db.config_context()
        .update::<_, DeskSignalError, _>(|candidate| {
            let current = &candidate.approval_gateway;
            if (
                current.configuration_revision,
                current.gateway.connection_revision,
                current.gateway.profile_revision,
            ) != (
                params.expected_configuration_revision,
                params.expected_connection_revision,
                params.expected_profile_revision,
            ) {
                return Err(DeskSignalError::new_custom_error(
                    DeskErrorCode::PRECONDITION_FAILED,
                    "approval model configuration revision conflict",
                ));
            }
            let source = &candidate.ai_gateway;
            if !source.is_configured() {
                return Err(DeskSignalError::new_custom_error(
                    DeskErrorCode::PRECONDITION_FAILED,
                    "AI gateway is not configured",
                ));
            }
            let mut copied = current.clone();
            let mut options = source.request_options.clone();
            // This independent connection has no approval-specific cache validation.
            desk_diagnose_core::prompt_cache::reset_connection(&mut options);
            copied.gateway.profile_schema_version = source.profile_schema_version;
            copied.apply_update(ApprovalModelUpdate {
                wire_protocol: source.wire_protocol,
                model: source.model.clone(),
                base_url: source.base_url.clone(),
                api_key: source.api_key.clone(),
                request_options: Some(options),
                output_limit_field: Some(source.output_limit_field),
                runtime_max_output_tokens: Some(source.runtime_max_output_tokens),
                max_context_bytes: source.max_context_bytes,
                ..Default::default()
            });
            copied.gateway.request_profile().map_err(|error| {
                DeskSignalError::new_custom_error(DeskErrorCode::INVALID_PARAMS, &error.to_string())
            })?;
            validate_provider_url(&copied)?;
            candidate.approval_gateway = copied;
            Ok(Some(()))
        })
        .await?;
    // Reuse the stored approval observation, including the unchanged-config case.
    super::load(db).await.map_err(Into::into)
}

#[cfg(test)]
mod tests;
