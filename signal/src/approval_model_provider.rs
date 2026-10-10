//! Separate OSS permission-review model configuration and optional probe observations.
//! Its configuration stays independent; the owner can explicitly copy the AI gateway.

mod reuse;
pub(crate) use reuse::validate_provider_url;
pub use reuse::{ApprovalModelReuseParams, reuse_ai_gateway};

use crate::config::connection::DatabaseConnection;
use desk_agent_protocol::ExecutionMode;
use desk_agent_protocol::data_lineage::DestinationIdentity;
use desk_diagnose_core::model_profile::{OutputLimitField, WireProtocol};
use sea_orm::sea_query::OnConflict;
use sea_orm::{ActiveValue::Set, DbErr, EntityTrait};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::config::ConfigConnection;
use crate::entity::approval_model_probe_observation as probe_row;
const SINGLETON_ID: i32 = 1;
use crate::model_provider::{ModelProbeObservation, ModelProviderConfig, ModelProviderUpdate};

#[derive(Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ApprovalModelConfig {
    pub enabled: bool,
    pub configuration_revision: i64,
    pub gateway: ModelProviderConfig,
    #[serde(skip)]
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
    pub reasoning_contract: desk_diagnose_core::model_profile::ReasoningContract,
    pub anthropic_prefix_binding: bool,
    #[schema(value_type = String)]
    pub output_limit_field: OutputLimitField,
    pub runtime_max_output_tokens: i64,
    pub max_context_bytes: Option<i64>,
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
    pub reasoning_contract: Option<desk_diagnose_core::model_profile::ReasoningContract>,
    pub anthropic_prefix_binding: Option<bool>,
    #[schema(value_type = Option<String>)]
    pub output_limit_field: Option<OutputLimitField>,
    pub runtime_max_output_tokens: Option<i64>,
    pub max_context_bytes: Option<i64>,
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
            reasoning_contract: self.gateway.reasoning_contract,
            anthropic_prefix_binding: self.gateway.anthropic_prefix_binding,
            output_limit_field: self.gateway.output_limit_field,
            runtime_max_output_tokens: self.gateway.runtime_max_output_tokens,
            max_context_bytes: self.gateway.max_context_bytes,
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
            connection_id: format!(
                "oss-approval-gateway:{SINGLETON_ID}:{}",
                self.gateway.config_instance
            ),
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
        let enabled_changed = update.enabled.is_some_and(|value| value != self.enabled);
        if let Some(enabled) = update.enabled {
            self.enabled = enabled;
        }
        self.gateway.apply_update(ModelProviderUpdate {
            wire_protocol: update.wire_protocol,
            model: update.model,
            supports_image_input: Some(false),
            base_url: update.base_url,
            api_key: update.api_key,
            request_options: update.request_options,
            reasoning_contract: update.reasoning_contract,
            anthropic_prefix_binding: update.anthropic_prefix_binding,
            output_limit_field: update.output_limit_field,
            runtime_max_output_tokens: update.runtime_max_output_tokens,
            max_context_bytes: update.max_context_bytes,
            ..Default::default()
        });
        let model_changed = self.gateway.connection_revision != prior_connection
            || self.gateway.profile_revision != prior_profile;
        if enabled_changed || model_changed {
            self.configuration_revision = self.configuration_revision.saturating_add(1).max(1);
        }
        if model_changed {
            self.probe_observation = None;
        }
    }
}

pub async fn load<C: crate::config::ConfigConnection>(
    db: &C,
) -> Result<ApprovalModelConfig, DbErr> {
    let snapshot = db.config_read().await;
    let mut config = snapshot.approval_gateway.clone();
    config.gateway.config_instance = snapshot.metadata.instance.clone();
    if let Some(observation) = probe_row::Entity::find_by_id(SINGLETON_ID).one(db).await? {
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
            // Completed tests validate the model connection and request profile.
            // Enablement still advances the configuration revision for live review checks.
            current: observation.config_instance == snapshot.metadata.instance
                && observation.connection_revision == config.gateway.connection_revision
                && observation.profile_revision == config.gateway.profile_revision,
        });
    }
    Ok(config)
}

pub async fn save_if_revisions_match(
    db: &DatabaseConnection,
    config: ApprovalModelConfig,
    expected_configuration_revision: i64,
    expected_connection_revision: i64,
    expected_profile_revision: i64,
) -> Result<bool, DbErr> {
    config
        .gateway
        .request_profile()
        .map_err(|error| DbErr::Custom(error.to_string()))?;
    Ok(db
        .config_context()
        .update::<_, DbErr, _>(|candidate| {
            let current = &candidate.approval_gateway;
            if (
                current.configuration_revision,
                current.gateway.connection_revision,
                current.gateway.profile_revision,
            ) != (
                expected_configuration_revision,
                expected_connection_revision,
                expected_profile_revision,
            ) {
                return Ok(None);
            }
            candidate.approval_gateway = config;
            Ok(Some(()))
        })
        .await?
        .is_some())
}

pub async fn save_probe_if_current(
    db: &DatabaseConnection,
    config: &ApprovalModelConfig,
    observation: ModelProbeObservation,
) -> Result<bool, DbErr> {
    let txn = crate::db::begin_write(db, probe_row::Entity).await?;
    let snapshot = txn.config_read().await;
    let current = &snapshot.approval_gateway;
    if snapshot.metadata.instance != config.gateway.config_instance
        || (
            current.configuration_revision,
            current.gateway.connection_revision,
            current.gateway.profile_revision,
        ) != (
            config.configuration_revision,
            config.gateway.connection_revision,
            config.gateway.profile_revision,
        )
    {
        drop(snapshot);
        txn.rollback().await?;
        return Ok(false);
    }
    probe_row::Entity::insert(probe_row::ActiveModel {
        approval_model_provider_id: Set(SINGLETON_ID),
        config_instance: Set(snapshot.metadata.instance.clone()),
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
                probe_row::Column::ConfigInstance,
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
    drop(snapshot);
    txn.commit().await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_diagnose_core::approval_review::REQUIRED_APPROVAL_PROBES;

    fn configured() -> ApprovalModelConfig {
        let mut value = ApprovalModelConfig::default();
        value.apply_update(ApprovalModelUpdate {
            enabled: Some(true),
            wire_protocol: Some(WireProtocol::OpenAiChatCompletions),
            model: Some("approval-model".into()),
            base_url: Some("https://example.test/v1".into()),
            api_key: Some("test-only-key".into()),
            ..Default::default()
        });
        value
    }

    fn probe(config: &ApprovalModelConfig) -> ModelProbeObservation {
        ModelProbeObservation {
            connection_revision: config.gateway.connection_revision,
            profile_revision: config.gateway.profile_revision,
            tested_at: chrono::Utc::now(),
            reasoning_observed: false,
            reasoning_tokens: None,
            stop_reason: Some("stop".into()),
            validated_capabilities: serde_json::Value::Object(
                REQUIRED_APPROVAL_PROBES
                    .iter()
                    .map(|name| ((*name).into(), serde_json::Value::Bool(true)))
                    .collect(),
            ),
            current: true,
        }
    }

    #[tokio::test]
    async fn oss_configuration_is_ready_without_probe_or_prices() {
        let db = crate::config::test_support::Database::connect("sqlite::memory:")
            .await
            .unwrap();
        crate::db::initialize_schema(&db).await.unwrap();
        let config = configured();
        assert_eq!(config.unavailable_reason(), None);
        assert!(
            save_if_revisions_match(&db, config.clone(), 1, 1, 1)
                .await
                .unwrap()
        );
        let config = load(&db).await.unwrap();
        assert!(config.public_view().available);
        assert!(config.probe_observation.is_none());
        assert!(config.destination_identity().is_ok());
        assert!(
            save_probe_if_current(&db, &config, probe(&config))
                .await
                .unwrap()
        );
        let mut loaded = load(&db).await.unwrap();
        assert!(loaded.public_view().available);
        assert!(loaded.destination_identity().is_ok());
        assert!(
            serde_json::to_value(loaded.public_view())
                .unwrap()
                .get("prices")
                .is_none()
        );
        loaded.apply_update(ApprovalModelUpdate {
            model: Some("different-model".into()),
            ..Default::default()
        });
        assert!(
            save_if_revisions_match(
                &db,
                loaded,
                config.configuration_revision,
                config.gateway.connection_revision,
                config.gateway.profile_revision
            )
            .await
            .unwrap()
        );
        assert!(
            !save_probe_if_current(&db, &config, probe(&config))
                .await
                .unwrap()
        );
        let changed = load(&db).await.unwrap();
        assert!(!changed.probe_observation.as_ref().unwrap().current);
        assert!(changed.public_view().available);
        assert!(changed.destination_identity().is_ok());
    }

    #[tokio::test]
    async fn empty_context_owner_can_open_approval_without_input_or_model_tests() {
        use crate::agent_session_store::{SignalAgentSessionStore, UpdateLiveContext};
        use desk_agent_protocol::ai_assistant::AiAssistantContextUpdate;
        use desk_diagnose_core::seam::SessionSeam;
        use desk_diagnose_core::{
            live_context::ContextSelectionClaim, session::AgentSessionSurface,
        };
        use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

        async fn persisted(
            db: &crate::config::connection::DatabaseConnection,
        ) -> desk_diagnose_core::session::PersistedAgentSession {
            let row = crate::entity::agent_session::Entity::find()
                .filter(crate::entity::agent_session::Column::ConversationId.eq("conversation"))
                .one(db)
                .await
                .unwrap()
                .unwrap();
            desk_diagnose_core::session::PersistedAgentSession::decode_json(&row.state_json)
                .unwrap()
        }

        let db = crate::config::test_support::Database::connect("sqlite::memory:")
            .await
            .unwrap();
        crate::db::initialize_schema(&db).await.unwrap();
        assert!(
            save_if_revisions_match(&db, configured(), 1, 1, 1)
                .await
                .unwrap()
        );
        assert!(load(&db).await.unwrap().probe_observation.is_none());
        let now = chrono::Utc::now();
        let store = SignalAgentSessionStore::new(db.clone()).with_client_metadata(
            Some("empty-client-conversation".into()),
            AgentSessionSurface::AiAssistant,
        );
        store
            .update_live_context(&UpdateLiveContext {
                run_id: "conversation".into(),
                actor_id: "1".into(),
                device_id: "device".into(),
                update: AiAssistantContextUpdate {
                    conversation_id: "empty-client-conversation".into(),
                    client_request_id: "initialize-empty".into(),
                    selected_capability_ids: vec![],
                },
                selection: Some(ContextSelectionClaim {
                    selected_capability_ids: vec![],
                    runtime_bindings: vec![],
                    candidates: vec![],
                    now_unix_ms: now.timestamp_millis() as u64,
                }),
                created_at: now.to_rfc3339(),
            })
            .await
            .unwrap();
        let session = persisted(&db).await;
        assert_eq!(session.input_revision, 0);
        assert_eq!(session.policy_revision, 0);
        assert!(session.conversation.is_empty());
        let delegation = crate::agent_approval_store::open_for_subject(
            &db,
            "conversation",
            "1",
            "device",
            session.input_revision,
            "owner-enable".into(),
            now.timestamp_millis() as u64,
        )
        .await
        .unwrap();
        assert!(
            crate::agent_approval_store::load_active_for_subject(
                &db,
                "conversation",
                "1",
                "device"
            )
            .await
            .unwrap()
            .is_some_and(|saved| saved.delegation_id == delegation.delegation_id)
        );
        assert!(load(&db).await.unwrap().probe_observation.is_none());
        let saved = persisted(&db).await;
        assert_eq!(saved.input_revision, 0);
        assert_eq!(saved.policy_revision, 0);
        assert!(saved.conversation.is_empty());
        assert!(saved.scope_snapshot.granted.is_empty());
        let message = desk_diagnose_core::model_message_labels::model_bound_user_message(
            "first-input".into(),
            "My first real message".into(),
            load(&db).await.unwrap().destination_identity().unwrap(),
        )
        .unwrap();
        crate::agent_run_event_store::SignalAgentRunEventStore::new(db.clone())
            .append_user_followup(crate::agent_run_event_store::AppendUserFollowupParams {
                event_id: "first-input-event".into(),
                run_id: "conversation".into(),
                client_conversation_id: Some("empty-client-conversation".into()),
                actor_id: "1".into(),
                device_id: "device".into(),
                surface: AgentSessionSurface::AiAssistant,
                policy_revision:
                    desk_diagnose_core::assistant_policy::PERSONAL_ASSISTANT_POLICY_REVISION,
                current_scope: saved.scope_snapshot,
                read_context: None,
                message,
                created_at: now.to_rfc3339(),
            })
            .await
            .unwrap();
        let started = persisted(&db).await;
        assert_eq!(started.input_revision, 1);
        assert_eq!(started.conversation.len(), 1);
        assert_eq!(started.policy_revision, 0);
        let claimed = store
            .claim_turn(desk_diagnose_core::seam::ClaimTurnParams {
                conversation_id: "conversation".into(),
                actor_id: "1".into(),
                device_id: "device".into(),
                policy_revision:
                    desk_diagnose_core::assistant_policy::PERSONAL_ASSISTANT_POLICY_REVISION,
                current_pdp_scope: started.scope_snapshot,
                turn_id: "first-turn".into(),
                request_id: Some("first-request".into()),
                connection_id: Some("browser".into()),
                now: now.to_rfc3339(),
                trigger_origin: desk_diagnose_core::session::TriggerOrigin::User,
            })
            .await
            .unwrap();
        assert_eq!(
            claimed.policy_revision,
            desk_diagnose_core::assistant_policy::PERSONAL_ASSISTANT_POLICY_REVISION
        );
        assert!(claimed.scope_snapshot.granted.is_empty());
        assert!(
            crate::agent_approval_store::load_active_for_subject(
                &db,
                "conversation",
                "1",
                "device"
            )
            .await
            .unwrap()
            .is_some_and(|saved| saved.delegation_id == delegation.delegation_id)
        );
    }

    #[test]
    fn runtime_budget_update_invalidates_approval_validation_and_has_no_probe_field() {
        let mut config = configured();
        config.probe_observation = Some(probe(&config));
        assert!(config.public_view().available);
        let previous_configuration = config.configuration_revision;
        let previous_profile = config.gateway.profile_revision;
        let previous_connection = config.gateway.connection_revision;
        config.apply_update(ApprovalModelUpdate {
            runtime_max_output_tokens: Some(8192),
            ..Default::default()
        });
        assert_eq!(config.configuration_revision, previous_configuration + 1);
        assert_eq!(config.gateway.profile_revision, previous_profile + 1);
        assert_eq!(config.gateway.connection_revision, previous_connection);
        assert!(config.probe_observation.is_none());
        assert_eq!(config.unavailable_reason(), None);
        let public = serde_json::to_value(config.public_view()).unwrap();
        assert_eq!(public["runtime_max_output_tokens"], 8192);
        assert!(public.get("probe_max_output_tokens").is_none());
    }

    #[tokio::test]
    async fn enabling_a_tested_model_preserves_its_probe_across_saved_toggles() {
        let db = crate::config::test_support::Database::connect("sqlite::memory:")
            .await
            .unwrap();
        crate::db::initialize_schema(&db).await.unwrap();
        let mut config = configured();
        config.apply_update(ApprovalModelUpdate {
            enabled: Some(false),
            ..Default::default()
        });
        assert!(save_if_revisions_match(&db, config, 1, 1, 1).await.unwrap());
        let tested = load(&db).await.unwrap();
        assert!(
            save_probe_if_current(&db, &tested, probe(&tested))
                .await
                .unwrap()
        );
        for enabled in [true, false, true] {
            let previous = load(&db).await.unwrap();
            let mut changed = previous.clone();
            changed.apply_update(ApprovalModelUpdate {
                enabled: Some(enabled),
                ..Default::default()
            });
            assert_eq!(
                changed.configuration_revision,
                previous.configuration_revision + 1
            );
            assert_eq!(
                changed.gateway.connection_revision,
                tested.gateway.connection_revision
            );
            assert_eq!(
                changed.gateway.profile_revision,
                tested.gateway.profile_revision
            );
            assert!(changed.probe_observation.as_ref().unwrap().current);
            assert_eq!(changed.public_view().available, enabled);
            assert!(
                save_if_revisions_match(
                    &db,
                    changed,
                    previous.configuration_revision,
                    previous.gateway.connection_revision,
                    previous.gateway.profile_revision,
                )
                .await
                .unwrap()
            );
            let saved = load(&db).await.unwrap();
            assert!(saved.probe_observation.as_ref().unwrap().current);
            assert_eq!(saved.public_view().available, enabled);
            assert_eq!(saved.destination_identity().is_ok(), enabled);
            assert_eq!(
                saved.unavailable_reason(),
                if enabled {
                    None
                } else {
                    Some("approval_model_disabled")
                }
            );
        }
        let recorded = probe_row::Entity::find_by_id(SINGLETON_ID)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            recorded.configuration_revision,
            tested.configuration_revision
        );
        // A result started before an enablement change still cannot overwrite the saved probe.
        assert!(
            !save_probe_if_current(&db, &tested, probe(&tested))
                .await
                .unwrap()
        );
        let current = load(&db).await.unwrap();
        probe_row::Entity::update(probe_row::ActiveModel {
            approval_model_provider_id: Set(SINGLETON_ID),
            config_instance: Set("different-config-instance".into()),
            ..Default::default()
        })
        .exec(&db)
        .await
        .unwrap();
        let stale = load(&db).await.unwrap();
        assert!(!stale.probe_observation.as_ref().unwrap().current);
        assert!(stale.public_view().available);
        assert!(
            save_probe_if_current(&db, &current, probe(&current))
                .await
                .unwrap()
        );
        assert!(load(&db).await.unwrap().public_view().available);
    }

    #[tokio::test]
    async fn connection_and_profile_changes_still_invalidate_a_completed_approval_probe() {
        for update in [
            ApprovalModelUpdate {
                base_url: Some("https://different.example.test/v1".into()),
                ..Default::default()
            },
            ApprovalModelUpdate {
                api_key: Some("different-test-only-key".into()),
                ..Default::default()
            },
            ApprovalModelUpdate {
                wire_protocol: Some(WireProtocol::AnthropicMessages),
                ..Default::default()
            },
            ApprovalModelUpdate {
                model: Some("different-review-model".into()),
                ..Default::default()
            },
            ApprovalModelUpdate {
                request_options: Some(serde_json::json!({"reasoning_effort": "high"})),
                ..Default::default()
            },
            ApprovalModelUpdate {
                output_limit_field: Some(OutputLimitField::MaxCompletionTokens),
                ..Default::default()
            },
            ApprovalModelUpdate {
                runtime_max_output_tokens: Some(8192),
                ..Default::default()
            },
            ApprovalModelUpdate {
                max_context_bytes: Some(65536),
                ..Default::default()
            },
        ] {
            let db = crate::config::test_support::Database::connect("sqlite::memory:")
                .await
                .unwrap();
            crate::db::initialize_schema(&db).await.unwrap();
            assert!(
                save_if_revisions_match(&db, configured(), 1, 1, 1)
                    .await
                    .unwrap()
            );
            let before = load(&db).await.unwrap();
            assert!(
                save_probe_if_current(&db, &before, probe(&before))
                    .await
                    .unwrap()
            );
            let mut after = load(&db).await.unwrap();
            after.apply_update(update);
            assert!(after.probe_observation.is_none());
            assert!(
                save_if_revisions_match(
                    &db,
                    after,
                    before.configuration_revision,
                    before.gateway.connection_revision,
                    before.gateway.profile_revision
                )
                .await
                .unwrap()
            );
            let saved = load(&db).await.unwrap();
            assert!(!saved.probe_observation.as_ref().unwrap().current);
            assert!(saved.public_view().available);
            assert!(saved.destination_identity().is_ok());
            assert!(
                !save_probe_if_current(&db, &before, probe(&before))
                    .await
                    .unwrap()
            );
        }
    }

    #[test]
    fn incomplete_or_stale_validation_does_not_gate_approval_but_enablement_does() {
        let mut config = configured();
        let mut observation = probe(&config);
        observation.validated_capabilities = serde_json::json!({});
        config.probe_observation = Some(observation);
        assert_eq!(config.unavailable_reason(), None);
        assert!(config.destination_identity().is_ok());
        config.probe_observation.as_mut().unwrap().current = false;
        assert_eq!(config.unavailable_reason(), None);
        assert!(config.destination_identity().is_ok());
        config.probe_observation = Some(probe(&config));
        config.apply_update(ApprovalModelUpdate {
            enabled: Some(false),
            ..Default::default()
        });
        assert_eq!(config.unavailable_reason(), Some("approval_model_disabled"));
        assert!(config.destination_identity().is_err());
        config.apply_update(ApprovalModelUpdate {
            enabled: Some(true),
            api_key: Some(String::new()),
            ..Default::default()
        });
        assert_eq!(
            config.unavailable_reason(),
            Some("approval_model_not_configured")
        );
        assert!(config.destination_identity().is_err());
    }
}
