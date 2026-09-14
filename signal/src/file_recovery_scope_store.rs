//! Trusted edge registration must be acknowledged before a file mutation starts.
use crate::entity::{agent_file_recovery_scope as vault, agent_session};
use sea_orm::{
    ActiveValue::Set,
    ColumnTrait, EntityTrait, QueryFilter, QueryOrder, TransactionTrait,
    sea_query::{Expr, OnConflict},
};
use sha2::{Digest, Sha256};

pub struct FileRecoveryScopeStore {
    db: sea_orm::DatabaseConnection,
}
impl FileRecoveryScopeStore {
    pub fn new(db: sea_orm::DatabaseConnection) -> Self {
        Self { db }
    }
    pub async fn prepare_dispatch(
        &self,
        connections: &desk_signal_facade::model::connection::SharedConnectionMap,
        connection: &str,
        actor: i32,
        device: Option<i32>,
        device_key: &str,
        conversation: &str,
    ) -> Result<desk_agent_protocol::authz::FileRecoveryRegistration, sea_orm::DbErr> {
        use desk_agent_protocol::file_recovery::*;
        let failed = || {
            sea_orm::DbErr::Custom(
                "Backup namespace registration unavailable; file was not changed".into(),
            )
        };
        let reply = desk_signal_facade::service::file_recovery::request_authorized(
            connections,
            connection,
            actor,
            device,
            FileRecoveryRequest {
                expected_authority: None,
                expected_os_user: None,
                command: FileRecoveryCommand::Query {
                    conversation_id: Some(conversation.into()),
                    after: None,
                },
            },
        )
        .await
        .map_err(|_| failed())?;
        let FileRecoveryOutcome::Page { page } = reply.outcome else {
            return Err(failed());
        };
        if !self
            .register(
                &actor.to_string(),
                device_key,
                conversation,
                &reply.authority,
                &reply.os_user,
                chrono::Utc::now().timestamp_millis(),
            )
            .await?
        {
            return Err(failed());
        }
        Ok(desk_agent_protocol::authz::FileRecoveryRegistration {
            execution_epoch: page.execution_epoch,
            authority: reply.authority,
            os_user: reply.os_user,
        })
    }

    /// Caller authenticates the originating worker and its work/owner binding first.
    /// A write lock on the conversation serializes registration with deletion.
    pub async fn register(
        &self,
        actor: &str,
        device: &str,
        conversation: &str,
        authority: &str,
        os_user: &str,
        now_ms: i64,
    ) -> Result<bool, sea_orm::DbErr> {
        if [actor, device, conversation]
            .iter()
            .any(|v| v.is_empty() || v.len() > 512 || v.chars().any(char::is_control))
            || authority.len() != 64
            || !authority
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || os_user.is_empty()
            || os_user.len() > 128
            || os_user.chars().any(char::is_control)
            || now_ms <= 0
        {
            return Err(sea_orm::DbErr::Custom(
                "invalid recovery scope registration".into(),
            ));
        }
        let id = format!(
            "{:x}",
            Sha256::digest(
                serde_json::to_vec(&(actor, device, conversation, authority, os_user))
                    .map_err(|_| sea_orm::DbErr::Custom("invalid recovery scope".into()))?
            )
        );
        let txn = self.db.begin().await?;
        let locked = agent_session::Entity::update_many()
            .col_expr(
                agent_session::Column::Version,
                Expr::col(agent_session::Column::Version).into(),
            )
            .filter(agent_session::Column::ConversationId.eq(conversation))
            .filter(agent_session::Column::ActorId.eq(actor))
            .filter(agent_session::Column::DeviceId.eq(device))
            .exec(&txn)
            .await?;
        if locked.rows_affected != 1 {
            txn.rollback().await?;
            return Ok(false);
        }
        vault::Entity::insert(vault::ActiveModel {
            id: Set(id),
            conversation_id: Set(conversation.into()),
            actor_id: Set(actor.into()),
            device_id: Set(device.into()),
            authority: Set(authority.into()),
            os_user: Set(os_user.into()),
            registered_at_unix_ms: Set(now_ms),
            cleaned_at_unix_ms: Set(None),
        })
        .on_conflict(
            OnConflict::column(vault::Column::Id)
                .do_nothing()
                .to_owned(),
        )
        .exec_without_returning(&txn)
        .await?;
        txn.commit().await?;
        Ok(true)
    }
    /// Only acknowledge the exact namespace returned by the authenticated device.
    /// A different user's empty vault must never complete this registration.
    pub async fn acknowledge_cleanup(
        &self,
        scope: &vault::Model,
        authority: &str,
        os_user: &str,
        now_ms: i64,
    ) -> Result<bool, sea_orm::DbErr> {
        if authority != scope.authority || os_user != scope.os_user || now_ms <= 0 {
            return Ok(false);
        }
        let updated = vault::Entity::update_many()
            .col_expr(vault::Column::CleanedAtUnixMs, Expr::value(now_ms))
            .filter(vault::Column::Id.eq(&scope.id))
            .filter(vault::Column::ActorId.eq(&scope.actor_id))
            .filter(vault::Column::DeviceId.eq(&scope.device_id))
            .filter(vault::Column::ConversationId.eq(&scope.conversation_id))
            .filter(vault::Column::Authority.eq(authority))
            .filter(vault::Column::OsUser.eq(os_user))
            .filter(vault::Column::CleanedAtUnixMs.is_null())
            .exec(&self.db)
            .await?;
        Ok(updated.rows_affected == 1)
    }

    pub async fn pending(
        &self,
        actor: &str,
        device: &str,
        conversation: &str,
    ) -> Result<Vec<vault::Model>, sea_orm::DbErr> {
        vault::Entity::find()
            .filter(vault::Column::ActorId.eq(actor))
            .filter(vault::Column::DeviceId.eq(device))
            .filter(vault::Column::ConversationId.eq(conversation))
            .filter(vault::Column::CleanedAtUnixMs.is_null())
            .order_by_asc(vault::Column::Id)
            .all(&self.db)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::{ActiveModelTrait, ConnectionTrait};
    #[tokio::test]
    async fn registrations_are_idempotent_scoped_and_fenced_by_deleted_conversations() {
        let db = sea_orm::Database::connect("sqlite::memory:").await.unwrap();
        let schema = sea_orm::Schema::new(db.get_database_backend());
        db.execute(
            &schema
                .create_table_from_entity(agent_session::Entity)
                .to_owned(),
        )
        .await
        .unwrap();
        db.execute(&schema.create_table_from_entity(vault::Entity).to_owned())
            .await
            .unwrap();
        let now = chrono::Utc::now();
        agent_session::ActiveModel {
            conversation_id: Set("conversation".into()),
            actor_id: Set("owner".into()),
            device_id: Set("device".into()),
            state_json: Set("{}".into()),
            version: Set(1),
            lease_token: Set(0),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(&db)
        .await
        .unwrap();
        let store = FileRecoveryScopeStore::new(db.clone());
        let authority = "a".repeat(64);
        assert!(
            !store
                .register("other", "device", "conversation", &authority, "501", 1)
                .await
                .unwrap()
        );
        assert!(
            !store
                .register("owner", "other", "conversation", &authority, "501", 1)
                .await
                .unwrap()
        );
        assert!(
            store
                .register("owner", "device", "conversation", &authority, "501", 1)
                .await
                .unwrap()
        );
        assert!(
            store
                .register("owner", "device", "conversation", &authority, "501", 2)
                .await
                .unwrap()
        );
        assert!(
            store
                .register("owner", "device", "conversation", &authority, "502", 3)
                .await
                .unwrap()
        );
        assert_eq!(
            store
                .pending("owner", "device", "conversation")
                .await
                .unwrap()
                .len(),
            2
        );
        agent_session::Entity::delete_many()
            .exec(&db)
            .await
            .unwrap();
        assert!(
            !store
                .register("owner", "device", "conversation", &authority, "503", 4)
                .await
                .unwrap()
        );
        let restarted = FileRecoveryScopeStore::new(db);
        assert_eq!(
            restarted
                .pending("owner", "device", "conversation")
                .await
                .unwrap()
                .len(),
            2
        );
        assert!(
            restarted
                .pending("other", "device", "conversation")
                .await
                .unwrap()
                .is_empty()
        );
        let registrations = restarted
            .pending("owner", "device", "conversation")
            .await
            .unwrap();
        let first = &registrations[0];
        assert!(
            !restarted
                .acknowledge_cleanup(first, &authority, "other-user", 5)
                .await
                .unwrap()
        );
        assert!(
            !restarted
                .acknowledge_cleanup(first, &"b".repeat(64), &first.os_user, 5)
                .await
                .unwrap()
        );
        assert_eq!(
            restarted
                .pending("owner", "device", "conversation")
                .await
                .unwrap()
                .len(),
            2
        );
        assert!(
            restarted
                .acknowledge_cleanup(first, &authority, &first.os_user, 5)
                .await
                .unwrap()
        );
        assert!(
            !restarted
                .acknowledge_cleanup(first, &authority, &first.os_user, 6)
                .await
                .unwrap()
        );
        let remaining = restarted
            .pending("owner", "device", "conversation")
            .await
            .unwrap();
        assert_eq!(remaining.len(), 1);
        assert_ne!(remaining[0].os_user, first.os_user);
    }
}
