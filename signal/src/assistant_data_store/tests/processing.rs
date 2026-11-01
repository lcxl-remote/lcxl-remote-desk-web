#[tokio::test]
async fn processing_status_reports_history_without_chat_bodies_or_obsolete_maintenance() {
    let (store, _, _, _) = super::model_gate::fixture("owner-processing-status").await;
    use crate::config::ConfigConnection;
    let config = store.db.config_read().await;
    let policy = config.assistant_data_policy.clone();
    let settings = config.assistant_data_settings.clone();
    let instance = Some(config.metadata.instance.clone());
    drop(config);
    let status = super::super::processing::read_on(
        &store.db,
        "1",
        instance.as_deref(),
        &settings,
        &policy,
        chrono::Utc::now().timestamp_millis() as u64,
    )
    .await
    .unwrap();

    let json = serde_json::to_value(&status).unwrap();
    assert!(json.get("maintenance").is_none());
    assert!(json.get("projections").is_none());
    assert!(!json.to_string().contains("Prefer Chinese"));
    let mut disabled = settings;
    disabled.automatic_memory_enabled = false;
    let status = super::super::processing::read_on(
        &store.db,
        "1",
        instance.as_deref(),
        &disabled,
        &policy,
        chrono::Utc::now().timestamp_millis() as u64,
    )
    .await
    .unwrap();
    assert_eq!(
        status.stored_history_fragments,
        json["stored_history_fragments"].as_u64().unwrap()
    );
}
