use super::*;
use crate::model::settings::{Args, Settings};
use desk_signal::config::{ConfigConnection, SECTION_NAMES};

#[tokio::test]
async fn reused_approval_credentials_survive_reload_and_runtime_database_rebuild() {
    use desk_signal::approval_model_provider::{self, ApprovalModelReuseParams};
    use desk_signal::config::connection::DatabaseConnection;
    use sea_orm::ConnectionTrait;

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("selected-profile.toml");
    let mut initial = Settings::for_test_config(&path);
    initial.global_config.initialize_metadata().unwrap();
    initial.save().unwrap();
    let configuration = context(Arc::new(SharedSettings::from(initial)))
        .await
        .unwrap();
    configuration
        .update::<_, DbErr, _>(|config| {
            config
                .ai_gateway
                .apply_update(desk_signal::model_provider::ModelProviderUpdate {
                    wire_protocol: Some(
                        desk_diagnose_core::model_profile::WireProtocol::OpenAiChatCompletions,
                    ),
                    model: Some("saved-main-model".into()),
                    base_url: Some("https://example.test/v1".into()),
                    api_key: Some("synthetic-main-key".into()),
                    request_options: Some(serde_json::json!({"reasoning_effort": "high"})),
                    runtime_max_output_tokens: Some(8192),
                    max_context_bytes: Some(262_144),
                    ..Default::default()
                });
            Ok(Some(()))
        })
        .await
        .unwrap();
    let db = DatabaseConnection::new(
        sea_orm::Database::connect("sqlite::memory:").await.unwrap(),
        configuration,
    );
    let schema = sea_orm::Schema::new(db.get_database_backend());
    db.execute(
        &schema.create_table_from_entity(
            desk_signal::entity::approval_model_probe_observation::Entity,
        ),
    )
    .await
    .unwrap();
    let current = approval_model_provider::load(&db).await.unwrap();
    approval_model_provider::reuse_ai_gateway(
        &db,
        &ApprovalModelReuseParams {
            expected_configuration_revision: current.configuration_revision,
            expected_connection_revision: current.gateway.connection_revision,
            expected_profile_revision: current.gateway.profile_revision,
        },
    )
    .await
    .unwrap();

    assert!(
        !std::fs::read_to_string(&path)
            .unwrap()
            .contains("probe_max_output_tokens")
    );
    let reloaded = Settings::load_readonly(&Args {
        config_file_path: Some(path),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(
        reloaded
            .global_config
            .approval_gateway
            .gateway
            .api_key
            .as_deref(),
        Some("synthetic-main-key")
    );
    assert!(!reloaded.global_config.approval_gateway.enabled);
    assert_eq!(
        reloaded
            .global_config
            .approval_gateway
            .gateway
            .runtime_max_output_tokens,
        8192
    );
    let restarted = context(Arc::new(SharedSettings::from(reloaded)))
        .await
        .unwrap();
    let rebuilt = DatabaseConnection::new(
        sea_orm::Database::connect("sqlite::memory:").await.unwrap(),
        restarted,
    );
    rebuilt
        .execute(&schema.create_table_from_entity(
            desk_signal::entity::approval_model_probe_observation::Entity,
        ))
        .await
        .unwrap();
    let copied = approval_model_provider::load(&rebuilt).await.unwrap();
    assert_eq!(copied.gateway.model.as_deref(), Some("saved-main-model"));
    assert_eq!(
        copied.gateway.request_options,
        serde_json::json!({"reasoning_effort": "high"})
    );
    assert_eq!(copied.gateway.max_context_bytes, Some(262_144));
    assert_eq!(copied.gateway.runtime_max_output_tokens, 8192);
    assert!(copied.probe_observation.is_none());
    rebuilt
        .config_context()
        .update::<_, DbErr, _>(|config| {
            config
                .ai_gateway
                .apply_update(desk_signal::model_provider::ModelProviderUpdate {
                    model: Some("later-main-model".into()),
                    api_key: Some("synthetic-later-key".into()),
                    ..Default::default()
                });
            Ok(Some(()))
        })
        .await
        .unwrap();
    let independent = approval_model_provider::load(&rebuilt).await.unwrap();
    assert_eq!(independent.gateway.model, copied.gateway.model);
    assert_eq!(independent.gateway.api_key, copied.gateway.api_key);
}

#[tokio::test]
async fn file_updates_and_host_updates_preserve_the_complete_profile_across_database_rebuilds() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("selected-profile.toml");
    let mut initial = Settings::for_test_config(&path);
    initial.global_config.initialize_metadata().unwrap();
    initial.save().unwrap();
    let settings = Arc::new(SharedSettings::from(initial));
    let configuration = context(settings.clone()).await.unwrap();
    configuration
        .update::<_, DbErr, _>(|config| {
            config.ai_gateway.model = Some("main-model".into());
            config.ai_gateway.api_key = Some("synthetic-main-secret".into());
            config.ai_gateway.wire_protocol =
                Some(desk_diagnose_core::model_profile::WireProtocol::AnthropicMessages);
            config.ai_gateway.request_options =
                serde_json::json!({"thinking": {"type": "adaptive", "display": null}});
            config.approval_gateway.enabled = true;
            config.approval_gateway.gateway.model = Some("approval-model".into());
            config.approval_gateway.gateway.api_key = Some("synthetic-approval-secret".into());
            config.web_search = config
                .web_search
                .candidate(&desk_signal_facade::web_search::SearchConfigUpdate {
                    expected_revision: config.web_search.revision,
                    provider: desk_signal_facade::web_search::SearchProvider::Brave,
                    api_key: Some("synthetic-search-secret".into()),
                })
                .unwrap();
            config.context_management.strategy =
                desk_diagnose_core::model_context::ContextManagementStrategy::Window;
            config.subagent_policy.limits.max_unfinished_per_root = 8;
            config.goal_budget_policy.limits.model_tokens = None;
            config.schedule_budget_policy.maximum.max_calls_per_run = 12;
            config.usage_retention.turn_days = 7;
            config.model_metrics.detail_row_budget = 200_000;
            config.model_metrics.compact_row_budget = 200_000;
            Ok(Some(()))
        })
        .await
        .unwrap();
    {
        let mut live = settings.write().await;
        let mut host_update = live.clone();
        host_update.log.log_level = "debug".into();
        host_update.system.locale = Some("zh-CN".into());
        host_update.save().unwrap();
        *live = host_update;
    }
    let encoded = std::fs::read_to_string(&path).unwrap();
    let document: toml::Value = toml::from_str(&encoded).unwrap();
    for section in SECTION_NAMES {
        assert!(document.get(section).is_some(), "missing {section}");
    }
    let args = Args {
        config_file_path: Some(path),
        ..Default::default()
    };
    let reloaded = Settings::new(&args).unwrap();
    assert_eq!(reloaded.log.log_level, "debug");
    let global = &reloaded.global_config;
    assert_eq!(global.ai_gateway.model.as_deref(), Some("main-model"));
    assert_eq!(
        global.ai_gateway.request_options["thinking"]["display"],
        serde_json::Value::Null
    );
    assert_eq!(
        global.approval_gateway.gateway.model.as_deref(),
        Some("approval-model")
    );
    assert!(global.web_search.public().has_api_key);
    assert_eq!(
        global.context_management.strategy,
        desk_diagnose_core::model_context::ContextManagementStrategy::Window
    );
    assert_eq!(global.subagent_policy.limits.max_unfinished_per_root, 8);
    assert!(global.goal_budget_policy.limits.model_tokens.is_none());
    assert_eq!(global.schedule_budget_policy.maximum.max_calls_per_run, 12);
    assert_eq!(global.usage_retention.turn_days, 7);
    assert_eq!(global.model_metrics.detail_row_budget, 200_000);
    let worker: Settings =
        serde_json::from_str(&serde_json::to_string(&reloaded).unwrap()).unwrap();
    assert_eq!(
        worker.global_config.ai_gateway.api_key,
        global.ai_gateway.api_key
    );
    assert_eq!(worker.global_config.model_metrics, global.model_metrics);
    assert!(!format!("{worker:?}").contains("synthetic-main-secret"));
    let restarted = context(Arc::new(SharedSettings::from(reloaded)))
        .await
        .unwrap();
    // Fresh runtime databases do not initialize or replace the persisted config.
    let db = desk_signal::config::connection::DatabaseConnection::new(
        sea_orm::Database::connect("sqlite::memory:").await.unwrap(),
        restarted,
    );
    use sea_orm::ConnectionTrait;
    let schema = sea_orm::Schema::new(db.get_database_backend());
    db.execute(
        &schema.create_table_from_entity(desk_signal::entity::model_probe_observation::Entity),
    )
    .await
    .unwrap();
    assert_eq!(
        db.config_read().await.ai_gateway.api_key.as_deref(),
        Some("synthetic-main-secret")
    );
    assert_eq!(db.config_read().await.usage_retention.turn_days, 7);
}

#[tokio::test]
async fn unrelated_host_and_global_saves_do_not_overwrite_each_other() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let mut initial = Settings::for_test_config(&path);
    initial.global_config.initialize_metadata().unwrap();
    initial.save().unwrap();
    let settings = Arc::new(SharedSettings::from(initial));
    let configuration = context(settings.clone()).await.unwrap();
    let global = configuration.update::<_, DbErr, _>(|config| {
        config.usage_retention.turn_days = 7;
        Ok(Some(()))
    });
    let host = async {
        let mut live = settings.write().await;
        let mut candidate = live.clone();
        candidate.log.log_level = "trace".into();
        candidate.save().unwrap();
        *live = candidate;
    };
    let (result, ()) = tokio::join!(global, host);
    result.unwrap();
    let reloaded = Settings::load_readonly(&Args {
        config_file_path: Some(path),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(reloaded.log.log_level, "trace");
    assert_eq!(reloaded.global_config.usage_retention.turn_days, 7);
}
