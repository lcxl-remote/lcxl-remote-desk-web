use super::*;
use sea_orm::TransactionTrait;

#[test]
fn gateways_persist_only_the_runtime_budget_and_edits_invalidate_their_probes() {
    let mut config = GlobalConfig::default();
    config.ai_gateway.runtime_max_output_tokens = 8192;
    config.approval_gateway.gateway.runtime_max_output_tokens = 128000;
    config.initialize_metadata().unwrap();
    let encoded = toml::to_string(&config).unwrap();
    assert!(!encoded.contains("probe_max_output_tokens"));
    let mut decoded: GlobalConfig = toml::from_str(&encoded).unwrap();
    assert_eq!(decoded.ai_gateway.runtime_max_output_tokens, 8192);
    assert_eq!(
        decoded.approval_gateway.gateway.runtime_max_output_tokens,
        128000
    );
    assert!(!decoded.initialize_metadata().unwrap());

    let edited = encoded.replace(
        "runtime_max_output_tokens = 128000",
        "runtime_max_output_tokens = 64000",
    );
    let mut decoded: GlobalConfig = toml::from_str(&edited).unwrap();
    assert!(decoded.initialize_metadata().unwrap());
    assert_eq!(
        decoded.ai_gateway.profile_revision,
        config.ai_gateway.profile_revision
    );
    assert_eq!(
        decoded.approval_gateway.gateway.profile_revision,
        config.approval_gateway.gateway.profile_revision + 1
    );
    assert_eq!(
        decoded.approval_gateway.configuration_revision,
        config.approval_gateway.configuration_revision + 1
    );
    assert_eq!(
        decoded.approval_gateway.gateway.connection_revision,
        config.approval_gateway.gateway.connection_revision
    );
}

#[test]
fn document_preserves_null_options_disabled_limits_and_decimal_revisions() {
    let mut config = GlobalConfig::default();
    config.ai_gateway.wire_protocol =
        Some(desk_diagnose_core::model_profile::WireProtocol::AnthropicMessages);
    config.ai_gateway.request_options =
        serde_json::json!({"thinking": {"type": "adaptive", "display": null}});
    config.ai_gateway.api_key = Some("synthetic-model-key".into());
    config.approval_gateway.gateway.api_key = Some("synthetic-review-key".into());
    config.goal_budget_policy.limits.model_tokens = None;
    config.subagent_policy.revision = u64::MAX;
    config.initialize_metadata().unwrap();
    let encoded = toml::to_string(&config).unwrap();
    assert!(encoded.contains("request_options_json"));
    assert!(encoded.contains("disabled"));
    assert!(encoded.contains(&format!("revision = \"{}\"", u64::MAX)));
    let decoded: GlobalConfig = toml::from_str(&encoded).unwrap();
    assert_eq!(
        decoded.ai_gateway.request_options,
        config.ai_gateway.request_options
    );
    assert_eq!(decoded.ai_gateway.api_key, config.ai_gateway.api_key);
    assert_eq!(
        decoded.approval_gateway.gateway.api_key,
        config.approval_gateway.gateway.api_key
    );
    assert_eq!(decoded.goal_budget_policy.limits.model_tokens, None);
    assert_eq!(decoded.subagent_policy.revision, u64::MAX);
    assert!(!format!("{decoded:?}").contains("synthetic-model-key"));
}

#[test]
fn missing_fields_use_business_defaults_and_invalid_fields_are_rejected() {
    let empty: GlobalConfig = toml::from_str("").unwrap();
    assert_eq!(empty.ai_gateway.runtime_max_output_tokens, 65_536);
    assert_eq!(
        empty.approval_gateway.gateway.runtime_max_output_tokens,
        65_536
    );
    assert_eq!(empty.context_management.summary_max_output_tokens, 16_384);
    assert_eq!(
        empty.goal_budget_policy.limits,
        desk_diagnose_core::goal_budget::initial().limits
    );
    assert_eq!(empty.model_metrics, MetricsSettings::defaults(false));
    for document in [
        "[ai_gateway]\nunexpected = true",
        "[ai_gateway]\nconnection_revision = 1",
        "[ai_gateway]\nrequest_options_json = '[]'",
        "[ai_gateway]\nrequest_options_json = '{broken secret}'",
        "[ai_gateway]\nprobe_max_output_tokens = 512",
        "[approval_gateway.gateway]\nprobe_max_output_tokens = 512",
        "[goal_budget_policy.limits]\nmodelTokens = 0",
        "[goal_budget_policy.limits]\nmodelTokens = 'unknown'",
    ] {
        assert!(toml::from_str::<GlobalConfig>(document).is_err());
    }
}

#[test]
fn file_edits_change_only_relevant_revisions_and_format_changes_do_not() {
    let mut original = GlobalConfig::default();
    original.initialize_metadata().unwrap();
    let encoded = toml::to_string(&original).unwrap();
    let mut decoded: GlobalConfig = toml::from_str(&(encoded + "\n# formatting only\n")).unwrap();
    assert!(!decoded.initialize_metadata().unwrap());
    decoded.ai_gateway.api_key = Some("new-synthetic-key".into());
    assert!(decoded.initialize_metadata().unwrap());
    assert_eq!(
        decoded.ai_gateway.connection_revision,
        original.ai_gateway.connection_revision + 1
    );
    assert_eq!(
        decoded.ai_gateway.profile_revision,
        original.ai_gateway.profile_revision
    );
    assert_eq!(decoded.web_search.revision, original.web_search.revision);
    let previous_instance = decoded.metadata.instance.clone();
    decoded.metadata = Metadata::default();
    decoded.initialize_metadata().unwrap();
    assert_ne!(decoded.metadata.instance, previous_instance);
}

#[tokio::test]
async fn transaction_reuses_its_snapshot_when_a_configuration_writer_is_waiting() {
    let db = test_support::Database::connect("sqlite::memory:")
        .await
        .unwrap();
    let txn = db.begin().await.unwrap();
    let first = txn.config_read().await;
    let writer_context = db.config_context().clone();
    let writer = tokio::spawn(async move {
        writer_context
            .update::<_, DbErr, _>(|config| {
                config.usage_retention.turn_days = 7;
                Ok(Some(()))
            })
            .await
    });
    tokio::task::yield_now().await;
    assert!(!writer.is_finished());
    let second = txn.config_read().await;
    assert!(Arc::ptr_eq(&first, &second));
    assert_eq!(second.usage_retention.turn_days, 30);
    drop(first);
    drop(second);
    txn.commit().await.unwrap();
    writer.await.unwrap().unwrap();
    assert_eq!(db.config_read().await.usage_retention.turn_days, 7);
}

#[tokio::test]
async fn policy_compare_and_swap_preserves_other_sections() {
    let db = test_support::Database::connect("sqlite::memory:")
        .await
        .unwrap();
    let initial = crate::subagent_policy::read(&db).await.unwrap();
    let request = desk_agent_protocol::ai_assistant::subagent_policy::UpdateSubAgentPolicy {
        expected_revision: initial.revision,
        limits: desk_agent_protocol::ai_assistant::subagent_policy::SubAgentLimits {
            max_unfinished_per_root: 8,
        },
    };
    let (first, second) = tokio::join!(
        crate::subagent_policy::update(&db, &request),
        crate::subagent_policy::update(&db, &request)
    );
    assert_ne!(first.is_ok(), second.is_ok());
    crate::usage_retention::save(
        &db,
        UsageRetentionConfig {
            turn_days: 7,
            agent_session_days: 14,
        },
    )
    .await
    .unwrap();
    let saved = db.config_read().await;
    assert_eq!(saved.subagent_policy.limits.max_unfinished_per_root, 8);
    assert_eq!(saved.usage_retention.turn_days, 7);
}

struct FailingFile;
#[async_trait]
impl ConfigPersistence for FailingFile {
    async fn persist(&self, _: &GlobalConfig) -> Result<(), DbErr> {
        Err(DbErr::Custom("test write failed before replacement".into()))
    }
}

#[tokio::test]
async fn write_failure_does_not_publish_candidate_configuration() {
    let mut initial = GlobalConfig::default();
    initial.initialize_metadata().unwrap();
    let context = ConfigContext::new(initial, Arc::new(FailingFile)).unwrap();
    assert!(
        context
            .update::<_, DbErr, _>(|config| {
                config.usage_retention.turn_days = 7;
                Ok(Some(()))
            })
            .await
            .is_err()
    );
    assert_eq!(context.read().await.usage_retention.turn_days, 30);
}

#[test]
fn stopped_file_edits_preserve_the_existing_prompt_cache_reset_rules() {
    let mut config = GlobalConfig::default();
    config.ai_gateway.wire_protocol =
        Some(desk_diagnose_core::model_profile::WireProtocol::AnthropicMessages);
    config.ai_gateway.request_options =
        serde_json::json!({"prompt_cache": {"mode": "anthropic_explicit", "cache_history": true}});
    config.initialize_metadata().unwrap();
    let original_profile = config.ai_gateway.profile_revision;
    config.ai_gateway.model = Some("edited-model".into());
    config.initialize_metadata().unwrap();
    assert_eq!(
        config.ai_gateway.request_options["prompt_cache"]["cache_history"],
        false
    );
    assert_eq!(config.ai_gateway.profile_revision, original_profile + 1);
    config.ai_gateway.api_key = Some("synthetic-key".into());
    config.initialize_metadata().unwrap();
    assert!(
        config
            .ai_gateway
            .request_options
            .get("prompt_cache")
            .is_none()
    );
}

#[tokio::test]
async fn rebuilding_runtime_databases_preserves_the_file_and_rebuilding_metadata_expires_probes() {
    use sea_orm::{ConnectionTrait, EntityTrait};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("signal.sqlite");
    let url = format!("sqlite://{}?mode=rwc", path.display());
    let db = test_support::Database::connect(&url).await.unwrap();
    crate::db::initialize_schema(&db).await.unwrap();
    crate::model_provider::save(
        &db,
        ModelProviderConfig {
            wire_protocol: Some(
                desk_diagnose_core::model_profile::WireProtocol::OpenAiChatCompletions,
            ),
            base_url: Some("https://model.example/v1".into()),
            model: Some("persisted-model".into()),
            api_key: Some("synthetic-restart-key".into()),
            max_context_bytes: Some(131_072),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let original = crate::model_provider::load(&db).await.unwrap();
    crate::terminal_completion_config::update(
        &db,
        &desk_signal_facade::terminal_completion::UpdateTerminalCompletionRequest {
            expected_revision: 0,
            max_output_tokens: 32768,
        },
    )
    .await
    .unwrap();
    crate::context_management_config::update(&db, &desk_signal_facade::context_management::UpdateContextManagementRequest {
        expected_revision: 0, summary_max_output_tokens: 128000,
        strategy: desk_signal_facade::context_management::ContextManagementStrategyDto::CheckpointSummary,
    }).await.unwrap();
    let observation = crate::model_provider::ModelProbeObservation {
        connection_revision: original.connection_revision,
        profile_revision: original.profile_revision,
        tested_at: chrono::Utc::now(),
        reasoning_observed: false,
        reasoning_tokens: None,
        stop_reason: Some("stop".into()),
        validated_capabilities: serde_json::json!({"text": true}),
        current: true,
    };
    assert!(
        crate::model_provider::save_probe_observation_if_current(
            &db,
            &original.config_instance,
            observation.clone()
        )
        .await
        .unwrap()
    );
    let old_seam = crate::model_dial::SignalModelSeam::from_config(&original).unwrap();
    db.close().await.unwrap();
    let file = path.with_extension("config.toml");
    let mut global: GlobalConfig =
        toml::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    global.metadata = Metadata::default();
    std::fs::write(&file, toml::to_string(&global).unwrap()).unwrap();
    let db = test_support::Database::connect(&url).await.unwrap();
    assert!(
        !crate::model_provider::load(&db)
            .await
            .unwrap()
            .probe_observation
            .unwrap()
            .current
    );
    assert!(
        !crate::model_provider::save_probe_observation_if_current(
            &db,
            &original.config_instance,
            observation
        )
        .await
        .unwrap()
    );
    assert!(old_seam.validate_current_on(&db).await.is_err());
    assert_eq!(
        crate::entity::model_probe_observation::Entity::find()
            .all(&db)
            .await
            .unwrap()
            .len(),
        1
    );
    db.close().await.unwrap();
    std::fs::remove_file(&path).unwrap();
    let db = test_support::Database::connect(&url).await.unwrap();
    crate::db::initialize_schema(&db).await.unwrap();
    assert_eq!(
        crate::terminal_completion_config::read(&db)
            .await
            .unwrap()
            .max_output_tokens,
        32768
    );
    assert_eq!(
        crate::context_management_config::read(&db)
            .await
            .unwrap()
            .summary_max_output_tokens,
        128000
    );
    assert_eq!(
        crate::model_provider::load(&db)
            .await
            .unwrap()
            .api_key
            .as_deref(),
        Some("synthetic-restart-key")
    );
    let metrics = crate::model_metrics::store::Store::new(db.clone(), false, "rebuild".into());
    crate::model_metrics::runtime::create_schema(&db)
        .await
        .unwrap();
    metrics.initialize_settings(1000).await.unwrap();
    let columns = db
        .query_all_raw(sea_orm::Statement::from_string(
            db.get_database_backend(),
            "PRAGMA table_info(model_metric_state)".to_owned(),
        ))
        .await
        .unwrap();
    assert!(columns.iter().all(|column| !matches!(
        column.try_get::<String>("", "name").unwrap().as_str(),
        "settings_json" | "revision"
    )));
    assert_eq!(
        metrics.load_settings().await.unwrap(),
        db.config_read().await.model_metrics
    );
}

#[test]
fn auxiliary_output_policies_round_trip_and_manual_edits_advance_revisions() {
    let mut config = GlobalConfig::default();
    config.context_management.summary_max_output_tokens = 32768;
    config.terminal_completion.max_output_tokens = 128000;
    config.initialize_metadata().unwrap();
    let document = toml::to_string(&config).unwrap();
    let mut reopened: GlobalConfig = toml::from_str(&document).unwrap();
    assert_eq!(reopened.context_management.summary_max_output_tokens, 32768);
    assert_eq!(reopened.terminal_completion.max_output_tokens, 128000);
    assert!(!reopened.initialize_metadata().unwrap());
    let changed = document.replace("max_output_tokens = 128000", "max_output_tokens = 64000");
    let mut edited: GlobalConfig = toml::from_str(&changed).unwrap();
    assert!(edited.initialize_metadata().unwrap());
    assert_eq!(
        edited.terminal_completion.revision,
        config.terminal_completion.revision + 1
    );
    for value in ["0", "-1", "1.5", "4294967296"] {
        assert!(
            toml::from_str::<GlobalConfig>(&format!(
                "[terminal_completion]\nmax_output_tokens = {value}"
            ))
            .is_err()
        );
    }
}
