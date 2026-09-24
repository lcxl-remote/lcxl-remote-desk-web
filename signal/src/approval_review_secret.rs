//! Stable installation-local HMAC key for review-context audit digests.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use sea_orm::sea_query::OnConflict;
use sea_orm::{ColumnTrait, DatabaseConnection, DbErr, EntityTrait, QueryFilter, Set};

use crate::entity::approval_review_secret;

pub async fn load_or_create(db: &DatabaseConnection) -> Result<[u8; 32], DbErr> {
    if let Some(key) = load(db).await? {
        return Ok(key);
    }
    let mut candidate = [0_u8; 32];
    candidate[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    candidate[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    let row = approval_review_secret::ActiveModel {
        id: Set(approval_review_secret::SINGLETON_ID),
        key_b64: Set(STANDARD.encode(candidate)),
    };
    match approval_review_secret::Entity::insert(row)
        .on_conflict(
            OnConflict::column(approval_review_secret::Column::Id)
                .do_nothing()
                .to_owned(),
        )
        .exec(db)
        .await
    {
        Ok(_) | Err(DbErr::RecordNotInserted) => {}
        Err(error) => return Err(error),
    }
    load(db)
        .await?
        .ok_or_else(|| DbErr::Custom("approval review secret vanished".into()))
}

async fn load(db: &DatabaseConnection) -> Result<Option<[u8; 32]>, DbErr> {
    let row = approval_review_secret::Entity::find()
        .filter(approval_review_secret::Column::Id.eq(approval_review_secret::SINGLETON_ID))
        .one(db)
        .await?;
    row.map(|row| {
        let bytes = STANDARD
            .decode(row.key_b64)
            .map_err(|_| DbErr::Custom("approval review secret is invalid".into()))?;
        bytes
            .try_into()
            .map_err(|_| DbErr::Custom("approval review secret length is invalid".into()))
    })
    .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::{ConnectionTrait, Database, Schema};

    #[tokio::test]
    async fn key_is_stable_and_different_installations_do_not_share_it() {
        let first = Database::connect("sqlite::memory:").await.unwrap();
        let second = Database::connect("sqlite::memory:").await.unwrap();
        for db in [&first, &second] {
            let schema = Schema::new(db.get_database_backend());
            db.execute(&schema.create_table_from_entity(approval_review_secret::Entity))
                .await
                .unwrap();
        }
        let key = load_or_create(&first).await.unwrap();
        assert_eq!(key, load_or_create(&first).await.unwrap());
        assert_ne!(key, load_or_create(&second).await.unwrap());
    }
}
