//! Device-code allocation; unique indexes arbitrate concurrent registrations.
use crate::{config::ConfigConnection, entity::device_code};
use desk_signal_facade::device_code::MAX_ATTEMPTS;
use sea_orm::{ColumnTrait, DbErr, EntityTrait, QueryFilter, QuerySelect, Set, SqlErr};

pub async fn get_or_create<C: ConfigConnection>(db: &C, client_id: &str) -> Result<String, DbErr> {
    get_or_create_with(db, client_id, desk_utils::string::generate_device_code).await
}
async fn get_or_create_with<C: ConfigConnection, F: FnMut(usize) -> String>(
    db: &C,
    client_id: &str,
    mut generate: F,
) -> Result<String, DbErr> {
    if let Some(row) = device_code::Entity::find()
        .filter(device_code::Column::ClientId.eq(client_id))
        .one(db)
        .await?
    {
        return Ok(row.device_code);
    }
    let length = db.config_read().await.device_codes.length as usize;
    for _ in 0..MAX_ATTEMPTS {
        let code = generate(length);
        let now = chrono::Utc::now();
        let candidate = device_code::ActiveModel {
            client_id: Set(client_id.to_owned()),
            device_code: Set(code.clone()),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        };
        match device_code::Entity::insert(candidate).exec(db).await {
            Ok(_) => return Ok(code),
            Err(error) => {
                if !matches!(error.sql_err(), Some(SqlErr::UniqueConstraintViolation(_))) {
                    return Err(error);
                }
                if let Some(row) = device_code::Entity::find()
                    .filter(device_code::Column::ClientId.eq(client_id))
                    .one(db)
                    .await?
                {
                    return Ok(row.device_code);
                }
                if device_code::Entity::find()
                    .filter(device_code::Column::DeviceCode.eq(code))
                    .one(db)
                    .await?
                    .is_none()
                {
                    return Err(error);
                }
            }
        }
    }
    Err(DbErr::Custom(
        "Could not allocate a unique device code after 5 attempts".into(),
    ))
}

/// A manually entered code is never silently replaced on collision.
pub async fn create<C: ConfigConnection>(
    db: &C,
    client_id: String,
    explicit: Option<String>,
    capabilities: Option<String>,
) -> Result<device_code::Model, DbErr> {
    use sea_orm::ActiveModelTrait;
    let length = if explicit.is_none() {
        Some(db.config_read().await.device_codes.length as usize)
    } else {
        None
    };
    for _ in 0..if explicit.is_some() { 1 } else { MAX_ATTEMPTS } {
        let code = explicit.clone().unwrap_or_else(|| {
            desk_utils::string::generate_device_code(length.expect("automatic length"))
        });
        let now = chrono::Utc::now();
        let candidate = device_code::ActiveModel {
            client_id: Set(client_id.clone()),
            device_code: Set(code.clone()),
            capabilities: Set(capabilities.clone()),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        };
        match candidate.insert(db).await {
            Ok(row) => return Ok(row),
            Err(error) => {
                if explicit.is_some()
                    || !matches!(error.sql_err(), Some(SqlErr::UniqueConstraintViolation(_)))
                {
                    return Err(error);
                }
                if device_code::Entity::find()
                    .filter(device_code::Column::ClientId.eq(&client_id))
                    .one(db)
                    .await?
                    .is_some()
                    || device_code::Entity::find()
                        .filter(device_code::Column::DeviceCode.eq(&code))
                        .one(db)
                        .await?
                        .is_none()
                {
                    return Err(error);
                }
            }
        }
    }
    Err(DbErr::Custom(
        "Could not allocate a unique device code after 5 attempts".into(),
    ))
}

/// Resolve against the durable current code and retain its exact grant snapshot.
pub async fn resolve_online<C: sea_orm::ConnectionTrait>(
    db: &C,
    connections: &crate::model::SharedConnectionMap,
    code: &str,
) -> Result<Option<(String, device_code::Model)>, DbErr> {
    let Some((id, client_id, created_at)) = device_code::Entity::find()
        .select_only()
        .columns([
            device_code::Column::Id,
            device_code::Column::ClientId,
            device_code::Column::CreatedAt,
        ])
        .filter(device_code::Column::DeviceCode.eq(code))
        .into_tuple::<(i32, String, chrono::DateTime<chrono::Utc>)>()
        .one(db)
        .await?
    else {
        return Ok(None);
    };
    let _identity = crate::device_code_lifecycle::global()
        .lock(&client_id)
        .await;
    // The first lookup only identifies the lock. Re-read the exact incarnation
    // under that lock so deletion/recreation cannot bind an old code snapshot
    // to a new connection, even if the owner reused the same code string.
    let Some(row) = device_code::Entity::find()
        .filter(device_code::Column::Id.eq(id))
        .filter(device_code::Column::ClientId.eq(client_id))
        .filter(device_code::Column::CreatedAt.eq(created_at))
        .filter(device_code::Column::DeviceCode.eq(code))
        .one(db)
        .await?
    else {
        return Ok(None);
    };
    let map = connections.read().await;
    Ok(map
        .current_oss_server(&row.client_id)
        .filter(|state| !state.lifecycle.is_retired())
        .map(|state| (state.model.connection_id.clone(), row)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::{ConnectionTrait, Schema};
    async fn db() -> crate::config::connection::DatabaseConnection {
        let db = crate::config::test_support::Database::connect("sqlite::memory:")
            .await
            .unwrap();
        db.execute(
            &Schema::new(db.get_database_backend()).create_table_from_entity(device_code::Entity),
        )
        .await
        .unwrap();
        db
    }
    #[tokio::test]
    async fn changed_length_only_applies_to_new_allocations_and_explicit_generation() {
        let db = db().await;
        let old = create(&db, "old".into(), Some("ABC234".into()), Some("{}".into()))
            .await
            .unwrap();
        crate::device_code_config::update(
            &db,
            &desk_signal_facade::device_code::UpdateDeviceCodeSettings {
                expected_revision: 0,
                length: 12,
            },
        )
        .await
        .unwrap();
        assert_eq!(get_or_create(&db, "old").await.unwrap(), "ABC234");
        let preserved = device_code::Entity::find_by_id(old.id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(preserved.generation, 0);
        assert_eq!(preserved.capabilities, old.capabilities);
        assert_eq!(get_or_create(&db, "new").await.unwrap().len(), 12);
        let (a, b) = tokio::join!(get_or_create(&db, "same"), get_or_create(&db, "same"));
        assert_eq!(a.unwrap(), b.unwrap());
    }
    #[tokio::test]
    async fn actual_insert_retries_code_collisions_and_bounds_exhaustion() {
        let db = db().await;
        create(&db, "taken".into(), Some("AAAAAAAA".into()), None)
            .await
            .unwrap();
        let mut candidates = ["AAAAAAAA", "BBBBBBBB"].into_iter();
        assert_eq!(
            get_or_create_with(&db, "new", |length| {
                assert_eq!(length, 8);
                candidates.next().unwrap().into()
            })
            .await
            .unwrap(),
            "BBBBBBBB"
        );
        let mut attempts = 0;
        assert!(
            get_or_create_with(&db, "exhausted", |_| {
                attempts += 1;
                "AAAAAAAA".into()
            })
            .await
            .is_err()
        );
        assert_eq!(attempts, 5);
        assert!(
            device_code::Entity::find()
                .filter(device_code::Column::ClientId.eq("exhausted"))
                .one(&db)
                .await
                .unwrap()
                .is_none()
        );
    }
    #[tokio::test]
    async fn storage_fault_is_not_a_collision_and_is_not_swallowed() {
        let db = db().await;
        db.execute_unprepared("CREATE TRIGGER reject_device BEFORE INSERT ON device_code BEGIN SELECT RAISE(FAIL, 'storage failure'); END;").await.unwrap();
        let mut attempts = 0;
        assert!(
            get_or_create_with(&db, "new", |_| {
                attempts += 1;
                "AAAAAAAA".into()
            })
            .await
            .is_err()
        );
        assert_eq!(attempts, 1);
    }
}
