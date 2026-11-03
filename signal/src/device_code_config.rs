//! File-owned settings for newly generated device codes.
use crate::config::{ConfigConnection, connection::DatabaseConnection};
use desk_signal_facade::device_code::DeviceCodeSettings;
use sea_orm::DbErr;

#[derive(Debug)]
pub enum WriteError {
    Conflict,
    Invalid,
    Db(DbErr),
}
impl From<DbErr> for WriteError {
    fn from(error: DbErr) -> Self {
        Self::Db(error)
    }
}

pub async fn read<C: ConfigConnection>(db: &C) -> Result<DeviceCodeSettings, DbErr> {
    Ok(db.config_read().await.device_codes)
}

pub async fn update(
    db: &DatabaseConnection,
    request: &desk_signal_facade::device_code::UpdateDeviceCodeSettings,
) -> Result<DeviceCodeSettings, WriteError> {
    db.config_context()
        .update(|config| {
            if config.device_codes.revision != request.expected_revision {
                return Err(WriteError::Conflict);
            }
            let next = config
                .device_codes
                .candidate(request.length)
                .map_err(|_| WriteError::Invalid)?;
            config.device_codes = next;
            Ok(Some(next))
        })
        .await?
        .ok_or(WriteError::Conflict)
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_signal_facade::device_code::UpdateDeviceCodeSettings;
    #[tokio::test]
    async fn configuration_rejects_invalid_or_stale_writes_and_persists_across_restart() {
        let directory = tempfile::tempdir().unwrap();
        let url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("codes.db").display()
        );
        let db = crate::config::test_support::Database::connect(&url)
            .await
            .unwrap();
        assert_eq!(read(&db).await.unwrap().length, 8);
        for length in [5, 65] {
            assert!(matches!(
                update(
                    &db,
                    &UpdateDeviceCodeSettings {
                        expected_revision: 0,
                        length
                    }
                )
                .await,
                Err(WriteError::Invalid)
            ));
        }
        let saved = update(
            &db,
            &UpdateDeviceCodeSettings {
                expected_revision: 0,
                length: 6,
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            update(
                &db,
                &UpdateDeviceCodeSettings {
                    expected_revision: 0,
                    length: 12
                }
            )
            .await,
            Err(WriteError::Conflict)
        ));
        drop(db);
        let reopened = crate::config::test_support::Database::connect(&url)
            .await
            .unwrap();
        assert_eq!(read(&reopened).await.unwrap(), saved);
    }
}
