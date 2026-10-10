use super::*;
use crate::approval_model_provider::{load, save_probe_if_current};
use crate::model_provider::{ModelProbeObservation, ModelProviderConfig, ModelProviderUpdate};
use desk_agent_protocol::ExecutionMode;
use desk_diagnose_core::{
    approval_review::REQUIRED_APPROVAL_PROBES,
    model_profile::{OutputLimitField, WireProtocol},
};
use sea_orm::DbErr;

fn source() -> ModelProviderConfig {
    let mut value = ModelProviderConfig::default();
    value.apply_update(ModelProviderUpdate {
        wire_protocol: Some(WireProtocol::OpenAiChatCompletions),
        model: Some("main-model".into()),
        base_url: Some("https://example.test/v1".into()),
        api_key: Some("synthetic-main-key".into()),
        request_options: Some(serde_json::json!({"reasoning_effort": "high"})),
        output_limit_field: Some(OutputLimitField::MaxCompletionTokens),
        runtime_max_output_tokens: Some(8192),
        max_context_bytes: Some(262_144),
        supports_image_input: Some(true),
        ..Default::default()
    });
    value.connection_revision = 41;
    value.profile_revision = 17;
    value
}

async fn database(enabled: bool) -> DatabaseConnection {
    let db = crate::config::test_support::Database::connect("sqlite::memory:")
        .await
        .unwrap();
    crate::db::initialize_schema(&db).await.unwrap();
    db.config_context()
        .update::<_, DbErr, _>(|config| {
            config.ai_gateway = source();
            config.approval_gateway.enabled = enabled;
            Ok(Some(()))
        })
        .await
        .unwrap();
    db
}

fn expected(config: &ApprovalModelConfig) -> ApprovalModelReuseParams {
    ApprovalModelReuseParams {
        expected_configuration_revision: config.configuration_revision,
        expected_connection_revision: config.gateway.connection_revision,
        expected_profile_revision: config.gateway.profile_revision,
    }
}

fn observation(config: &ApprovalModelConfig) -> ModelProbeObservation {
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
async fn copies_credentials_and_profile_but_preserves_independent_approval_settings() {
    for enabled in [false, true] {
        let db = database(enabled).await;
        let original = serde_json::to_value(&db.config_read().await.ai_gateway).unwrap();
        let current = load(&db).await.unwrap();
        let copied = reuse_ai_gateway(&db, &expected(&current)).await.unwrap();
        assert_eq!(copied.enabled, enabled);
        let gateway = &copied.gateway;
        assert_eq!(gateway.model.as_deref(), Some("main-model"));
        assert_eq!(gateway.base_url.as_deref(), Some("https://example.test/v1"));
        assert_eq!(gateway.api_key.as_deref(), Some("synthetic-main-key"));
        assert_eq!(
            gateway.wire_protocol,
            Some(WireProtocol::OpenAiChatCompletions)
        );
        assert_eq!(
            gateway.request_options,
            serde_json::json!({"reasoning_effort": "high"})
        );
        assert_eq!(
            gateway.output_limit_field,
            OutputLimitField::MaxCompletionTokens
        );
        assert_eq!(gateway.runtime_max_output_tokens, 8192);
        assert_eq!(gateway.max_context_bytes, Some(262_144));
        assert_eq!(
            gateway.profile_schema_version,
            source().profile_schema_version
        );
        assert_eq!(gateway.execution_mode, ExecutionMode::SuggestOnly);
        assert!(!gateway.supports_image_input);
        assert_ne!(gateway.connection_revision, 41);
        assert_ne!(gateway.profile_revision, 17);
        assert!(copied.probe_observation.is_none());
        assert_eq!(copied.public_view().available, enabled);
        let public = serde_json::to_value(copied.public_view()).unwrap();
        assert_eq!(public["api_key_set"], true);
        assert!(public.get("api_key").is_none());
        assert!(!public.to_string().contains("synthetic-main-key"));
        assert_eq!(
            serde_json::to_value(&db.config_read().await.ai_gateway).unwrap(),
            original
        );
    }
}

#[tokio::test]
async fn rejects_incomplete_sources_and_conflicting_target_revisions_without_publishing() {
    let db = database(false).await;
    let current = load(&db).await.unwrap();
    for field in 0..3 {
        let mut params = expected(&current);
        match field {
            0 => params.expected_configuration_revision += 1,
            1 => params.expected_connection_revision += 1,
            2 => params.expected_profile_revision += 1,
            _ => unreachable!(),
        }
        assert!(reuse_ai_gateway(&db, &params).await.is_err());
    }
    db.config_context()
        .update::<_, DbErr, _>(|config| {
            config.ai_gateway.api_key = None;
            Ok(Some(()))
        })
        .await
        .unwrap();
    assert!(reuse_ai_gateway(&db, &expected(&current)).await.is_err());
    assert_eq!(
        serde_json::to_value(&db.config_read().await.approval_gateway).unwrap(),
        serde_json::to_value(&current).unwrap()
    );
}

#[tokio::test]
async fn rejects_invalid_provider_url_without_changing_approval() {
    let db = database(false).await;
    let current = load(&db).await.unwrap();
    db.config_context()
        .update::<_, DbErr, _>(|config| {
            config.ai_gateway.base_url = Some("not-a-provider-url".into());
            Ok(Some(()))
        })
        .await
        .unwrap();
    assert!(reuse_ai_gateway(&db, &expected(&current)).await.is_err());
    assert_eq!(
        load(&db).await.unwrap().configuration_revision,
        current.configuration_revision
    );
}

#[tokio::test]
async fn resets_prompt_cache_for_the_independent_approval_connection() {
    let db = database(false).await;
    db.config_context()
        .update::<_, DbErr, _>(|config| {
            config.ai_gateway.apply_update(ModelProviderUpdate {
                wire_protocol: Some(WireProtocol::AnthropicMessages),
                output_limit_field: Some(OutputLimitField::MaxTokens),
                request_options: Some(serde_json::json!({
                    "thinking": {"type": "adaptive", "display": null}
                })),
                ..Default::default()
            });
            // Simulate a cache configuration validated after connecting the main gateway.
            config.ai_gateway.request_options["prompt_cache"] = serde_json::json!({
                "mode": "anthropic_explicit", "cache_history": true
            });
            Ok(Some(()))
        })
        .await
        .unwrap();
    let current = load(&db).await.unwrap();
    let copied = reuse_ai_gateway(&db, &expected(&current)).await.unwrap();
    assert!(copied.gateway.request_options.get("prompt_cache").is_none());
    assert_eq!(
        copied.gateway.request_options["thinking"]["type"],
        "adaptive"
    );
    assert!(
        db.config_read()
            .await
            .ai_gateway
            .request_options
            .get("prompt_cache")
            .is_some()
    );
}

#[tokio::test]
async fn unchanged_copies_keep_approval_validation_but_changes_reject_late_probes() {
    let db = database(true).await;
    let current = load(&db).await.unwrap();
    let copied = reuse_ai_gateway(&db, &expected(&current)).await.unwrap();
    assert!(
        save_probe_if_current(&db, &copied, observation(&copied))
            .await
            .unwrap()
    );
    let ready = load(&db).await.unwrap();
    assert!(ready.public_view().available);
    let unchanged = reuse_ai_gateway(&db, &expected(&ready)).await.unwrap();
    assert_eq!(
        unchanged.configuration_revision,
        ready.configuration_revision
    );
    assert!(unchanged.public_view().available);
    db.config_context()
        .update::<_, DbErr, _>(|config| {
            config.ai_gateway.apply_update(ModelProviderUpdate {
                model: Some("new-main-model".into()),
                ..Default::default()
            });
            Ok(Some(()))
        })
        .await
        .unwrap();
    let changed = reuse_ai_gateway(&db, &expected(&unchanged)).await.unwrap();
    assert!(changed.public_view().available);
    assert!(!changed.probe_observation.as_ref().unwrap().current);
    assert!(
        !save_probe_if_current(&db, &ready, observation(&ready))
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn reads_the_source_after_preceding_writers_and_rejects_a_concurrent_target_copy() {
    let db = database(false).await;
    let current = load(&db).await.unwrap();
    let context = db.config_context().clone();
    let guard = context.read().await;
    let writer = tokio::spawn({
        let context = context.clone();
        async move {
            context
                .update::<_, DbErr, _>(|config| {
                    config.ai_gateway.apply_update(ModelProviderUpdate {
                        model: Some("latest-model".into()),
                        api_key: Some("synthetic-latest-key".into()),
                        ..Default::default()
                    });
                    Ok(Some(()))
                })
                .await
                .unwrap()
        }
    });
    tokio::task::yield_now().await;
    let copy = tokio::spawn({
        let db = db.clone();
        let params = expected(&current);
        async move { reuse_ai_gateway(&db, &params).await.unwrap() }
    });
    tokio::task::yield_now().await;
    drop(guard);
    writer.await.unwrap();
    let copied = copy.await.unwrap();
    assert_eq!(copied.gateway.model.as_deref(), Some("latest-model"));
    assert_eq!(
        copied.gateway.api_key.as_deref(),
        Some("synthetic-latest-key")
    );
    assert!(reuse_ai_gateway(&db, &expected(&current)).await.is_err());
}
