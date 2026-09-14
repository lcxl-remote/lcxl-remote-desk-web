//! Durable retry queue. Every update is fenced by the exact lease token.
use crate::entity::agent_file_recovery_cleanup as cleanup;
use sea_orm::{
    ColumnTrait, Condition, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder, QuerySelect,
    sea_query::Expr,
};

pub struct FileRecoveryCleanupStore {
    db: DatabaseConnection,
}
impl FileRecoveryCleanupStore {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    /// Bring a pending job forward without disturbing an active worker lease.
    pub async fn request_retry(
        &self,
        actor: &str,
        conversation: &str,
        now_ms: i64,
    ) -> Result<bool, sea_orm::DbErr> {
        let result = cleanup::Entity::update_many()
            .col_expr(cleanup::Column::NextAttemptAtUnixMs, Expr::value(now_ms))
            .filter(cleanup::Column::ActorId.eq(actor))
            .filter(cleanup::Column::ConversationId.eq(conversation))
            .filter(cleanup::Column::CompletedAtUnixMs.is_null())
            .filter(
                Condition::any()
                    .add(cleanup::Column::LeaseUntilUnixMs.is_null())
                    .add(cleanup::Column::LeaseUntilUnixMs.lte(now_ms)),
            )
            .exec(&self.db)
            .await?;
        Ok(result.rows_affected == 1)
    }

    /// Read durable pending work without requiring a live device connection.
    pub async fn pending_for_actor(
        &self,
        actor: &str,
        after: Option<&str>,
    ) -> Result<Vec<cleanup::Model>, sea_orm::DbErr> {
        let mut query = cleanup::Entity::find()
            .filter(cleanup::Column::ActorId.eq(actor))
            .filter(cleanup::Column::CompletedAtUnixMs.is_null());
        if let Some(after) = after {
            query = query.filter(cleanup::Column::ConversationId.gt(after));
        }
        query
            .order_by_asc(cleanup::Column::ConversationId)
            .limit(101)
            .all(&self.db)
            .await
    }

    /// Device routing and owner authorization are the caller's responsibility.
    pub async fn pending_for_device(
        &self,
        device: &str,
        actor: &str,
    ) -> Result<Vec<cleanup::Model>, sea_orm::DbErr> {
        cleanup::Entity::find()
            .filter(cleanup::Column::DeviceId.eq(device))
            .filter(cleanup::Column::ActorId.eq(actor))
            .filter(cleanup::Column::CompletedAtUnixMs.is_null())
            .order_by_asc(cleanup::Column::CreatedAtUnixMs)
            .limit(100)
            .all(&self.db)
            .await
    }

    pub async fn claim_due(&self, now_ms: i64) -> Result<Option<cleanup::Model>, sea_orm::DbErr> {
        self.claim_matching(None, now_ms).await
    }
    pub async fn claim_for_device(
        &self,
        device: &str,
        actor: &str,
        now_ms: i64,
    ) -> Result<Option<cleanup::Model>, sea_orm::DbErr> {
        self.claim_matching(Some((device, actor)), now_ms).await
    }
    async fn claim_matching(
        &self,
        subject: Option<(&str, &str)>,
        now_ms: i64,
    ) -> Result<Option<cleanup::Model>, sea_orm::DbErr> {
        let available = || {
            Condition::all()
                .add(cleanup::Column::CompletedAtUnixMs.is_null())
                .add(cleanup::Column::NextAttemptAtUnixMs.lte(now_ms))
                .add(
                    Condition::any()
                        .add(cleanup::Column::LeaseUntilUnixMs.is_null())
                        .add(cleanup::Column::LeaseUntilUnixMs.lte(now_ms)),
                )
        };
        let mut candidates = cleanup::Entity::find().filter(available());
        if let Some((device, actor)) = subject {
            candidates = candidates
                .filter(cleanup::Column::DeviceId.eq(device))
                .filter(cleanup::Column::ActorId.eq(actor));
        }
        let candidates = candidates
            .order_by_asc(cleanup::Column::NextAttemptAtUnixMs)
            .limit(16)
            .all(&self.db)
            .await?;
        for candidate in candidates {
            let lease = uuid::Uuid::new_v4().to_string();
            let changed = cleanup::Entity::update_many()
                .col_expr(cleanup::Column::LeaseId, Expr::value(&lease))
                .col_expr(
                    cleanup::Column::LeaseUntilUnixMs,
                    Expr::value(now_ms.saturating_add(120_000)),
                )
                .col_expr(
                    cleanup::Column::Attempts,
                    Expr::value(candidate.attempts.saturating_add(1)),
                )
                .filter(cleanup::Column::ConversationId.eq(&candidate.conversation_id))
                .filter(cleanup::Column::Attempts.eq(candidate.attempts))
                .filter(available())
                .exec(&self.db)
                .await?;
            if changed.rows_affected == 1 {
                return cleanup::Entity::find_by_id(candidate.conversation_id)
                    .filter(cleanup::Column::LeaseId.eq(lease))
                    .one(&self.db)
                    .await;
            }
        }
        Ok(None)
    }

    /// Only a matching, unexpired lease can acknowledge cleanup of the device tombstone.
    pub async fn complete(
        &self,
        conversation: &str,
        lease: &str,
        now_ms: i64,
    ) -> Result<bool, sea_orm::DbErr> {
        let updated = cleanup::Entity::update_many()
            .col_expr(cleanup::Column::CompletedAtUnixMs, Expr::value(now_ms))
            .col_expr(
                cleanup::Column::LeaseId,
                Expr::value(Option::<String>::None),
            )
            .col_expr(
                cleanup::Column::LeaseUntilUnixMs,
                Expr::value(Option::<i64>::None),
            )
            .col_expr(
                cleanup::Column::LastError,
                Expr::value(Option::<String>::None),
            )
            .filter(lease_filter(conversation, lease, now_ms))
            .exec(&self.db)
            .await?;
        Ok(updated.rows_affected == 1)
    }

    /// Store a bounded category only, never a device path or remote error body.
    pub async fn retry(
        &self,
        job: &cleanup::Model,
        category: &str,
        now_ms: i64,
    ) -> Result<bool, sea_orm::DbErr> {
        if !matches!(
            category,
            "offline" | "timeout" | "identity_changed" | "cleanup_pending" | "unavailable"
        ) {
            return Err(sea_orm::DbErr::Custom(
                "Invalid file cleanup failure category".into(),
            ));
        }
        let Some(lease) = job.lease_id.as_deref() else {
            return Ok(false);
        };
        let delay_ms = (5_000i64 << job.attempts.clamp(0, 6) as u32).min(300_000);
        let updated = cleanup::Entity::update_many()
            .col_expr(
                cleanup::Column::NextAttemptAtUnixMs,
                Expr::value(now_ms.saturating_add(delay_ms)),
            )
            .col_expr(cleanup::Column::LastError, Expr::value(category))
            .col_expr(
                cleanup::Column::LeaseId,
                Expr::value(Option::<String>::None),
            )
            .col_expr(
                cleanup::Column::LeaseUntilUnixMs,
                Expr::value(Option::<i64>::None),
            )
            .filter(lease_filter(&job.conversation_id, lease, now_ms))
            .exec(&self.db)
            .await?;
        Ok(updated.rows_affected == 1)
    }
}
fn lease_filter(conversation: &str, lease: &str, now_ms: i64) -> Condition {
    Condition::all()
        .add(cleanup::Column::ConversationId.eq(conversation))
        .add(cleanup::Column::LeaseId.eq(lease))
        .add(cleanup::Column::LeaseUntilUnixMs.gt(now_ms))
        .add(cleanup::Column::CompletedAtUnixMs.is_null())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::{ActiveModelTrait, ConnectionTrait, Set};
    #[tokio::test]
    async fn cleanup_lease_survives_restart_and_fences_old_acknowledgments() {
        let db = sea_orm::Database::connect("sqlite::memory:").await.unwrap();
        db.execute(
            &sea_orm::Schema::new(db.get_database_backend())
                .create_table_from_entity(cleanup::Entity)
                .to_owned(),
        )
        .await
        .unwrap();
        cleanup::ActiveModel {
            conversation_id: Set("conversation".into()),
            actor_id: Set("owner".into()),
            device_id: Set("device".into()),
            created_at_unix_ms: Set(1000),
            next_attempt_at_unix_ms: Set(1000),
            attempts: Set(0),
            lease_id: Set(None),
            lease_until_unix_ms: Set(None),
            completed_at_unix_ms: Set(None),
            last_error: Set(None),
        }
        .insert(&db)
        .await
        .unwrap();
        let first = FileRecoveryCleanupStore::new(db.clone());
        assert_eq!(
            first.pending_for_actor("owner", None).await.unwrap().len(),
            1
        );
        assert!(
            first
                .pending_for_actor("other", None)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            first
                .pending_for_actor("owner", Some("conversation"))
                .await
                .unwrap()
                .is_empty()
        );
        let job = first.claim_due(1000).await.unwrap().unwrap();
        let second = FileRecoveryCleanupStore::new(db);
        assert!(second.claim_due(1001).await.unwrap().is_none());
        assert!(
            second
                .pending_for_device("device", "other")
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            !second
                .request_retry("owner", "conversation", 1001)
                .await
                .unwrap()
        );
        assert!(
            !second
                .request_retry("other", "conversation", 121_000)
                .await
                .unwrap()
        );
        let replacement = second.claim_due(121_000).await.unwrap().unwrap();
        assert_ne!(job.lease_id, replacement.lease_id);
        assert!(
            !first
                .complete("conversation", job.lease_id.as_deref().unwrap(), 121_001)
                .await
                .unwrap()
        );
        assert!(!first.retry(&job, "offline", 121_001).await.unwrap());
        assert!(
            second
                .retry(&replacement, "offline", 121_001)
                .await
                .unwrap()
        );
        assert!(second.claim_due(121_002).await.unwrap().is_none());
        assert!(
            second
                .request_retry("owner", "conversation", 121_002)
                .await
                .unwrap()
        );
        let retry = second.claim_due(121_002).await.unwrap().unwrap();
        assert_eq!(retry.attempts, 3);
        assert!(
            second
                .complete("conversation", retry.lease_id.as_deref().unwrap(), 121_003)
                .await
                .unwrap()
        );
        assert!(
            second
                .pending_for_device("device", "owner")
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            second
                .pending_for_actor("owner", None)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            !second
                .request_retry("owner", "conversation", 1_000_000)
                .await
                .unwrap()
        );
        assert!(second.claim_due(1_000_000).await.unwrap().is_none());
    }
}
