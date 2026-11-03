//! File-owned access-code protection settings.
use crate::config::{ConfigConnection, connection::DatabaseConnection};
use desk_signal_facade::access_code_protection::{
    AccessCodeProtectionSettings, UpdateAccessCodeProtectionSettings,
};
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

pub async fn read<C: ConfigConnection>(db: &C) -> Result<AccessCodeProtectionSettings, DbErr> {
    Ok(db.config_read().await.access_code_protection)
}
pub async fn update(
    db: &DatabaseConnection,
    request: &UpdateAccessCodeProtectionSettings,
) -> Result<AccessCodeProtectionSettings, WriteError> {
    db.config_context()
        .update(|config| {
            if config.access_code_protection.revision != request.expected_revision {
                return Err(WriteError::Conflict);
            }
            // Classify bad request values before the complete document is persisted.
            request
                .policy
                .validate(false)
                .map_err(|_| WriteError::Invalid)?;
            let next = AccessCodeProtectionSettings {
                policy: request.policy,
                revision: config
                    .access_code_protection
                    .revision
                    .checked_add(1)
                    .ok_or(WriteError::Invalid)?,
            };
            config.access_code_protection = next;
            Ok(Some(next))
        })
        .await?
        .ok_or(WriteError::Conflict)
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_signal_facade::access_code_protection::AccessCodeProtectionPolicy;
    #[tokio::test]
    async fn settings_persist_and_reject_invalid_or_stale_writes() {
        let directory = tempfile::tempdir().unwrap();
        let url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("policy.db").display()
        );
        let db = crate::config::test_support::Database::connect(&url)
            .await
            .unwrap();
        let initial = read(&db).await.unwrap();
        let policy = AccessCodeProtectionPolicy {
            network_max_attempts: 3,
            max_failures: 2,
            ..initial.policy
        };
        let updated = update(
            &db,
            &UpdateAccessCodeProtectionSettings {
                expected_revision: 0,
                policy,
            },
        )
        .await
        .unwrap();
        assert_eq!(updated.revision, 1);
        assert!(matches!(
            update(
                &db,
                &UpdateAccessCodeProtectionSettings {
                    expected_revision: 0,
                    policy
                }
            )
            .await,
            Err(WriteError::Conflict)
        ));
        for policy in [
            AccessCodeProtectionPolicy {
                network_max_attempts: 0,
                ..policy
            },
            AccessCodeProtectionPolicy::manager_default(),
        ] {
            assert!(matches!(
                update(
                    &db,
                    &UpdateAccessCodeProtectionSettings {
                        expected_revision: 1,
                        policy
                    }
                )
                .await,
                Err(WriteError::Invalid)
            ));
        }
        assert_eq!(read(&db).await.unwrap(), updated);
        drop(db);
        let reopened = crate::config::test_support::Database::connect(&url)
            .await
            .unwrap();
        assert_eq!(read(&reopened).await.unwrap(), updated);
    }
}
