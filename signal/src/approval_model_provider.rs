//! Separate OSS permission-review model configuration and current probe gate.
//! The conversational model's singleton row and credential are never reused.

use desk_agent_protocol::ExecutionMode;
use desk_agent_protocol::data_lineage::DestinationIdentity;
use desk_diagnose_core::approval_cost::ApprovalTokenPrices;
use desk_diagnose_core::approval_review::has_complete_approval_probe;
use desk_diagnose_core::model_profile::{
    MODEL_PROFILE_SCHEMA_VERSION, OutputLimitField, WireProtocol,
};
use sea_orm::sea_query::{Expr, OnConflict};
use sea_orm::{
    ActiveValue::Set, ColumnTrait, DatabaseConnection, DbErr, EntityTrait, QueryFilter,
    TryInsertResult,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::entity::{
    approval_model_probe_observation as probe_row, approval_model_provider as provider_row,
};
use crate::model_provider::{ModelProbeObservation, ModelProviderConfig, ModelProviderUpdate};

#[derive(Clone)]
pub struct ApprovalModelConfig {
    pub enabled: bool,
    pub configuration_revision: i64,
    pub gateway: ModelProviderConfig,
    pub prices: Option<ApprovalTokenPrices>,
    pub probe_observation: Option<ModelProbeObservation>,
}

impl Default for ApprovalModelConfig {
    fn default() -> Self {
        let mut gateway = ModelProviderConfig::default();
        gateway.execution_mode = ExecutionMode::SuggestOnly;
        gateway.supports_image_input = false;
        gateway.max_context_bytes = Some(131_072);
        Self {
            enabled: false,
            configuration_revision: 1,
            gateway,
            prices: None,
            probe_observation: None,
        }
    }
}

#[derive(Clone, Serialize, ToSchema)]
pub struct ApprovalModelPublic {
    pub enabled: bool,
    pub configuration_revision: i64,
    #[schema(value_type = Option<String>)]
    pub wire_protocol: Option<WireProtocol>,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub api_key_set: bool,
    #[schema(value_type = Object)]
    pub request_options: serde_json::Value,
    #[schema(value_type = String)]
    pub output_limit_field: OutputLimitField,
    pub probe_max_output_tokens: i64,
    pub runtime_max_output_tokens: i64,
    pub max_context_bytes: Option<i64>,
    #[schema(value_type = Option<Object>)]
    pub prices: Option<ApprovalTokenPrices>,
    pub connection_revision: i64,
    pub profile_revision: i64,
    pub probe_observation: Option<ModelProbeObservation>,
    pub available: bool,
    pub unavailable_reason: Option<String>,
}

#[derive(Clone, Deserialize, ToSchema, Default)]
pub struct ApprovalModelUpdate {
    pub expected_configuration_revision: i64,
    pub expected_connection_revision: i64,
    pub expected_profile_revision: i64,
    pub enabled: Option<bool>,
    #[schema(value_type = Option<String>)]
    pub wire_protocol: Option<WireProtocol>,
    pub model: Option<String>,
    pub base_url: Option<String>,
    /// Write-only: absent means keep, empty means clear.
    pub api_key: Option<String>,
    #[schema(value_type = Object)]
    pub request_options: Option<serde_json::Value>,
    #[schema(value_type = Option<String>)]
    pub output_limit_field: Option<OutputLimitField>,
    pub probe_max_output_tokens: Option<i64>,
    pub runtime_max_output_tokens: Option<i64>,
    pub max_context_bytes: Option<i64>,
    #[schema(value_type = Option<Object>)]
    pub prices: Option<ApprovalTokenPrices>,
}

impl ApprovalModelConfig {
    pub fn public_view(&self) -> ApprovalModelPublic {
        let unavailable_reason = self.unavailable_reason();
        ApprovalModelPublic {
            enabled: self.enabled,
            configuration_revision: self.configuration_revision,
            wire_protocol: self.gateway.wire_protocol,
            model: self.gateway.model.clone(),
            base_url: self.gateway.base_url.clone(),
            api_key_set: self.gateway.api_key_set(),
            request_options: self.gateway.request_options.clone(),
            output_limit_field: self.gateway.output_limit_field,
            probe_max_output_tokens: self.gateway.probe_max_output_tokens,
            runtime_max_output_tokens: self.gateway.runtime_max_output_tokens,
            max_context_bytes: self.gateway.max_context_bytes,
            prices: self.prices,
            connection_revision: self.gateway.connection_revision,
            profile_revision: self.gateway.profile_revision,
            probe_observation: self.probe_observation.clone(),
            available: unavailable_reason.is_none(),
            unavailable_reason: unavailable_reason.map(str::to_owned),
        }
    }

    pub fn unavailable_reason(&self) -> Option<&'static str> {
        if !self.enabled {
            return Some("approval_model_disabled");
        }
        if !self.gateway.is_configured() {
            return Some("approval_model_not_configured");
        }
        let Some(probe) = self.probe_observation.as_ref() else {
            return Some("approval_model_not_tested");
        };
        if !probe.current {
            return Some("approval_model_probe_stale");
        }
        if !has_complete_approval_probe(&probe.validated_capabilities) {
            return Some("approval_model_probe_incomplete");
        }
        if self
            .prices
            .and_then(ApprovalTokenPrices::validate)
            .is_none()
        {
            return Some("approval_model_price_not_configured");
        }
        None
    }

    pub fn destination_identity(&self) -> Result<DestinationIdentity, &'static str> {
        if self.unavailable_reason().is_some() {
            return Err("approval model is not available");
        }
        let connection_revision = u64::try_from(self.gateway.connection_revision)
            .map_err(|_| "invalid approval model connection revision")?;
        let model_id = self
            .gateway
            .model
            .as_deref()
            .ok_or("approval model is missing")?;
        let identity = DestinationIdentity::Model {
            connection_id: format!("oss-approval-gateway:{}", provider_row::SINGLETON_ID),
            connection_revision,
            model_id: model_id.to_owned(),
            profile_revision: self.gateway.profile_revision,
        };
        identity
            .validate()
            .map_err(|_| "invalid approval model destination")?;
        Ok(identity)
    }

    pub fn apply_update(&mut self, update: ApprovalModelUpdate) {
        let prior_connection = self.gateway.connection_revision;
        let prior_profile = self.gateway.profile_revision;
        let prior_prices = self.prices;
        let enabled_changed = update.enabled.is_some_and(|value| value != self.enabled);
        if let Some(enabled) = update.enabled {
            self.enabled = enabled;
        }
        if let Some(prices) = update.prices {
            self.prices = prices.validate();
        }
        self.gateway.apply_update(ModelProviderUpdate {
            wire_protocol: update.wire_protocol,
            model: update.model,
            supports_image_input: Some(false),
            base_url: update.base_url,
            api_key: update.api_key,
            request_options: update.request_options,
            output_limit_field: update.output_limit_field,
            probe_max_output_tokens: update.probe_max_output_tokens,
            runtime_max_output_tokens: update.runtime_max_output_tokens,
            max_context_bytes: update.max_context_bytes,
            ..Default::default()
        });
        if enabled_changed
            || self.prices != prior_prices
            || self.gateway.connection_revision != prior_connection
            || self.gateway.profile_revision != prior_profile
        {
            self.configuration_revision = self.configuration_revision.saturating_add(1).max(1);
            self.probe_observation = None;
        }
    }

    fn from_row(row: provider_row::Model) -> Result<Self, DbErr> {
        let mut config = Self::default();
        config.enabled = row.enabled;
        config.configuration_revision = row.configuration_revision;
        config.gateway.wire_protocol = row
            .wire_protocol
            .as_deref()
            .map(WireProtocol::parse)
            .transpose()
            .map_err(|error| DbErr::Custom(error.to_string()))?;
        config.gateway.model = row.model;
        config.gateway.base_url = row.base_url;
        config.gateway.api_key = row.api_key;
        config.gateway.profile_schema_version = u16::try_from(row.profile_schema_version)
            .map_err(|_| DbErr::Custom("invalid approval profile schema version".into()))?;
        config.gateway.request_options = serde_json::from_str(&row.request_options)
            .map_err(|_| DbErr::Custom("invalid approval request options".into()))?;
        config.gateway.output_limit_field = OutputLimitField::parse(&row.output_limit_field)
            .map_err(|error| DbErr::Custom(error.to_string()))?;
        config.gateway.probe_max_output_tokens = row.probe_max_output_tokens;
        config.gateway.runtime_max_output_tokens = row.runtime_max_output_tokens;
        config.gateway.max_context_bytes = Some(row.max_context_bytes);
        config.gateway.connection_revision = row.connection_revision;
        config.gateway.profile_revision = row.profile_revision;
        config.prices = row
            .prices_json
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(|_| DbErr::Custom("invalid approval model prices".into()))?;
        Ok(config)
    }

    fn into_row(self) -> Result<provider_row::ActiveModel, DbErr> {
        let profile = self
            .gateway
            .request_profile()
            .map_err(|error| DbErr::Custom(error.to_string()))?;
        Ok(provider_row::ActiveModel {
            id: Set(provider_row::SINGLETON_ID),
            enabled: Set(self.enabled),
            wire_protocol: Set(self.gateway.wire_protocol.map(|value| value.to_string())),
            model: Set(self.gateway.model),
            base_url: Set(self.gateway.base_url),
            api_key: Set(self.gateway.api_key),
            profile_schema_version: Set(i32::from(MODEL_PROFILE_SCHEMA_VERSION)),
            request_options: Set(profile.request_options.to_string()),
            output_limit_field: Set(profile.output_limit_field.to_string()),
            probe_max_output_tokens: Set(profile.probe_max_output_tokens),
            runtime_max_output_tokens: Set(profile.runtime_max_output_tokens),
            max_context_bytes: Set(profile.max_context_bytes),
            prices_json: Set(self
                .prices
                .map(|prices| serde_json::to_string(&prices))
                .transpose()
                .map_err(|_| DbErr::Custom("invalid approval model prices".into()))?),
            connection_revision: Set(self.gateway.connection_revision),
            profile_revision: Set(self.gateway.profile_revision),
            configuration_revision: Set(self.configuration_revision),
            updated_at: Set(chrono::Utc::now()),
        })
    }
}

pub async fn load<C: sea_orm::ConnectionTrait>(db: &C) -> Result<ApprovalModelConfig, DbErr> {
    let Some(row) = provider_row::Entity::find_by_id(provider_row::SINGLETON_ID)
        .one(db)
        .await?
    else {
        return Ok(ApprovalModelConfig::default());
    };
    let mut config = ApprovalModelConfig::from_row(row)?;
    if let Some(observation) = probe_row::Entity::find_by_id(provider_row::SINGLETON_ID)
        .one(db)
        .await?
    {
        let capabilities = serde_json::from_str(&observation.validated_capabilities)
            .map_err(|_| DbErr::Custom("invalid approval probe capabilities".into()))?;
        config.probe_observation = Some(ModelProbeObservation {
            connection_revision: observation.connection_revision,
            profile_revision: observation.profile_revision,
            tested_at: observation.tested_at,
            reasoning_observed: observation.reasoning_observed.unwrap_or(false),
            reasoning_tokens: observation.reasoning_tokens,
            stop_reason: observation.stop_reason,
            validated_capabilities: capabilities,
            current: observation.connection_revision == config.gateway.connection_revision
                && observation.profile_revision == config.gateway.profile_revision
                && observation.configuration_revision == config.configuration_revision,
        });
    }
    Ok(config)
}

/// The configuration revision also fences disable/re-enable cycles, while the
/// connection/profile revisions pin the exact dial target and request profile.
pub async fn save_if_revisions_match(
    db: &DatabaseConnection,
    config: ApprovalModelConfig,
    expected_configuration_revision: i64,
    expected_connection_revision: i64,
    expected_profile_revision: i64,
) -> Result<bool, DbErr> {
    let row = config.into_row()?;
    let updated = provider_row::Entity::update_many()
        .set(row.clone())
        .filter(provider_row::Column::Id.eq(provider_row::SINGLETON_ID))
        .filter(provider_row::Column::ConfigurationRevision.eq(expected_configuration_revision))
        .filter(provider_row::Column::ConnectionRevision.eq(expected_connection_revision))
        .filter(provider_row::Column::ProfileRevision.eq(expected_profile_revision))
        .exec(db)
        .await?;
    if updated.rows_affected == 1 {
        return Ok(true);
    }
    if (
        expected_configuration_revision,
        expected_connection_revision,
        expected_profile_revision,
    ) != (1, 1, 1)
    {
        return Ok(false);
    }
    let inserted = provider_row::Entity::insert(row)
        .on_conflict_do_nothing()
        .exec_without_returning(db)
        .await?;
    Ok(matches!(inserted, TryInsertResult::Inserted(1)))
}

pub async fn save_probe_if_current(
    db: &DatabaseConnection,
    config: &ApprovalModelConfig,
    observation: ModelProbeObservation,
) -> Result<bool, DbErr> {
    let txn = crate::db::begin_write(db, provider_row::Entity).await?;
    let matched = provider_row::Entity::update_many()
        .col_expr(
            provider_row::Column::Id,
            Expr::col(provider_row::Column::Id),
        )
        .filter(provider_row::Column::Id.eq(provider_row::SINGLETON_ID))
        .filter(provider_row::Column::ConfigurationRevision.eq(config.configuration_revision))
        .filter(provider_row::Column::ConnectionRevision.eq(config.gateway.connection_revision))
        .filter(provider_row::Column::ProfileRevision.eq(config.gateway.profile_revision))
        .exec(&txn)
        .await?
        .rows_affected
        == 1;
    if !matched {
        txn.rollback().await?;
        return Ok(false);
    }
    probe_row::Entity::insert(probe_row::ActiveModel {
        approval_model_provider_id: Set(provider_row::SINGLETON_ID),
        connection_revision: Set(config.gateway.connection_revision),
        profile_revision: Set(config.gateway.profile_revision),
        configuration_revision: Set(config.configuration_revision),
        tested_at: Set(observation.tested_at),
        reasoning_observed: Set(Some(observation.reasoning_observed)),
        reasoning_tokens: Set(observation.reasoning_tokens),
        stop_reason: Set(observation.stop_reason),
        validated_capabilities: Set(observation.validated_capabilities.to_string()),
    })
    .on_conflict(
        OnConflict::column(probe_row::Column::ApprovalModelProviderId)
            .update_columns([
                probe_row::Column::ConnectionRevision,
                probe_row::Column::ProfileRevision,
                probe_row::Column::ConfigurationRevision,
                probe_row::Column::TestedAt,
                probe_row::Column::ReasoningObserved,
                probe_row::Column::ReasoningTokens,
                probe_row::Column::StopReason,
                probe_row::Column::ValidatedCapabilities,
            ])
            .to_owned(),
    )
    .exec(&txn)
    .await?;
    txn.commit().await?;
    Ok(true)
}
