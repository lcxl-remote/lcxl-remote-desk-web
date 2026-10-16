//! File-owned execution policy for independent approval calls.
use crate::config::{ConfigConnection, connection::DatabaseConnection};
use desk_signal_facade::model::approval_runtime::ApprovalRuntimeSettings;
use sea_orm::DbErr;
pub async fn load<C: ConfigConnection>(db: &C) -> Result<ApprovalRuntimeSettings, DbErr> {
    let value = db.config_read().await.approval_runtime.clone();
    value.validate().map_err(|e| DbErr::Custom(e.into()))?;
    Ok(value)
}
pub async fn save(
    db: &DatabaseConnection,
    mut value: ApprovalRuntimeSettings,
) -> Result<Option<ApprovalRuntimeSettings>, DbErr> {
    value.validate().map_err(|e| DbErr::Custom(e.into()))?;
    let expected = value.revision.clone();
    value.revision = expected
        .parse::<u64>()
        .ok()
        .and_then(|v| v.checked_add(1))
        .ok_or_else(|| DbErr::Custom("approval runtime revision exhausted".into()))?
        .to_string();
    db.config_context()
        .update::<_, DbErr, _>(|config| {
            if config.approval_runtime.revision != expected {
                return Ok(None);
            }
            config.approval_runtime = value.clone();
            Ok(Some(value))
        })
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn retry_policy_is_file_owned_with_conflict_detection_and_restart_persistence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime.db");
        let url = format!("sqlite://{}?mode=rwc", path.display());
        let db = crate::config::test_support::Database::connect(&url)
            .await
            .unwrap();
        let initial = load(&db).await.unwrap();
        assert_eq!(initial.format_retry_count, 1);
        let mut edited = initial.clone();
        edited.format_retry_count = 0;
        let saved = save(&db, edited).await.unwrap().unwrap();
        assert_ne!(saved.revision, initial.revision);
        assert!(save(&db, initial).await.unwrap().is_none());
        let contents = std::fs::read_to_string(path.with_extension("config.toml")).unwrap();
        assert!(contents.contains("[approval_runtime]"));
        assert!(contents.contains("format_retry_count = 0"));
        db.close().await.unwrap();
        let restarted = crate::config::test_support::Database::connect(&url)
            .await
            .unwrap();
        assert_eq!(load(&restarted).await.unwrap(), saved);
    }
}
