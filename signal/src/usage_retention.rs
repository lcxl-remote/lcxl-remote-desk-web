//! Usage-retention configuration for the OSS signal server.
//!
//! The portable signal server is single-node and single-account, so there is
//! exactly one retention config (the singleton row in
//! [`crate::entity::usage_retention`]). It controls how many days of
//! `turn_usage_hourly` / `ai_usage_hourly` rollups and idle AI diagnosis
//! conversations are kept before the cleanup loop deletes them.
//!
//! Unlike the manager's cluster-shared config (optimistic-concurrency `revision`
//! for multi-instance safety), the signal server never runs multi-instance, so
//! writes are plain last-write-wins insert-or-replace on the fixed primary key —
//! no revision, no conflict semantics.

use std::time::Duration;

use chrono::Utc;
use sea_orm::ActiveValue::Set;
use sea_orm::prelude::DateTimeUtc;
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseConnection, DbErr, EntityTrait, QueryFilter, QueryOrder,
    QuerySelect, QueryTrait, Statement, TransactionTrait, Value,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::entity::usage_retention::{self, SINGLETON_ID};
use crate::entity::{
    agent_action_item, agent_capability_dispatch_outbox, agent_capability_grant, agent_exec_task,
    agent_grant_reservation, agent_run_event, agent_session,
};

/// Default retention window (days) when no row has been written yet.
pub const DEFAULT_RETENTION_DAYS: u32 = 30;
/// Smallest accepted retention window (days).
pub const MIN_RETENTION_DAYS: u32 = 1;
/// Largest accepted retention window (days) — ~27 years, a sane fat-finger guard.
pub const MAX_RETENTION_DAYS: u32 = 10_000;

/// Usage-retention windows, one per rollup family, in whole days.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct UsageRetentionConfig {
    /// Retention window for TURN traffic rollups (`turn_usage_hourly`), in days.
    pub turn_days: u32,
    /// Retention window for AI token rollups (`ai_usage_hourly`), in days.
    pub ai_days: u32,
    /// Idle retention window for AI diagnosis conversations, in days.
    pub agent_session_days: u32,
}

impl Default for UsageRetentionConfig {
    fn default() -> Self {
        Self {
            turn_days: DEFAULT_RETENTION_DAYS,
            ai_days: DEFAULT_RETENTION_DAYS,
            agent_session_days: DEFAULT_RETENTION_DAYS,
        }
    }
}

impl UsageRetentionConfig {
    fn from_entity(row: usage_retention::Model) -> Self {
        Self {
            turn_days: row.turn_days.max(0) as u32,
            ai_days: row.ai_days.max(0) as u32,
            agent_session_days: row.agent_session_days.max(0) as u32,
        }
    }

    /// Reject out-of-range windows before persisting.
    pub fn validate(&self) -> Result<(), String> {
        for (label, days) in [
            ("turn_days", self.turn_days),
            ("ai_days", self.ai_days),
            ("agent_session_days", self.agent_session_days),
        ] {
            if days < MIN_RETENTION_DAYS {
                return Err(format!(
                    "{label} must be at least {MIN_RETENTION_DAYS} day(s)"
                ));
            }
            if days > MAX_RETENTION_DAYS {
                return Err(format!("{label} must be at most {MAX_RETENTION_DAYS} days"));
            }
        }
        Ok(())
    }

    fn into_active_model(self) -> usage_retention::ActiveModel {
        usage_retention::ActiveModel {
            id: Set(SINGLETON_ID),
            turn_days: Set(self.turn_days.min(i32::MAX as u32) as i32),
            ai_days: Set(self.ai_days.min(i32::MAX as u32) as i32),
            agent_session_days: Set(self.agent_session_days.min(i32::MAX as u32) as i32),
            updated_at: Set(chrono::Utc::now()),
        }
    }
}

/// Load the singleton retention config, returning the default when no row has
/// been written yet.
pub async fn load(db: &DatabaseConnection) -> Result<UsageRetentionConfig, DbErr> {
    let row = usage_retention::Entity::find_by_id(SINGLETON_ID)
        .one(db)
        .await?;
    Ok(row
        .map(UsageRetentionConfig::from_entity)
        .unwrap_or_default())
}

/// Persist the singleton retention config (insert-or-replace on the fixed PK).
pub async fn save(db: &DatabaseConnection, config: UsageRetentionConfig) -> Result<(), DbErr> {
    use sea_orm::sea_query::OnConflict;
    let active = config.into_active_model();
    usage_retention::Entity::insert(active)
        .on_conflict(
            OnConflict::column(usage_retention::Column::Id)
                .update_columns([
                    usage_retention::Column::TurnDays,
                    usage_retention::Column::AiDays,
                    usage_retention::Column::AgentSessionDays,
                    usage_retention::Column::UpdatedAt,
                ])
                .to_owned(),
        )
        .exec(db)
        .await?;
    Ok(())
}

// ---- Retention cleanup ----

/// Distinct hour buckets deleted per DELETE batch (bounds lock duration).
const CLEANUP_BATCH_HOURS: u64 = 24;
/// Upper bound on DELETE batches per table per tick, so a large initial backfill
/// drains over several ticks instead of one unbounded pass.
const CLEANUP_MAX_BATCHES_PER_TICK: usize = 500;
/// Cleanup tick cadence.
const CLEANUP_INTERVAL: Duration = Duration::from_secs(3600);
const AGENT_SESSION_BATCH_ROWS: u64 = 1000;

const TURN_USAGE_TABLE: &str = "turn_usage_hourly";
const AI_USAGE_TABLE: &str = "ai_usage_hourly";

/// Delete rollup rows in `table` older than `cutoff`, in bounded batches. The
/// signal rollups are collect-only telemetry with no billing coupling, so cleanup
/// deletes purely by age. Returns rows deleted.
async fn cleanup_table(
    db: &DatabaseConnection,
    table: &str,
    cutoff: DateTimeUtc,
) -> Result<u64, DbErr> {
    let backend = db.get_database_backend();
    let mut total = 0u64;
    for _ in 0..CLEANUP_MAX_BATCHES_PER_TICK {
        // Delete all rows for the oldest batch of expired hours. The subquery bounds
        // each statement to `CLEANUP_BATCH_HOURS` distinct hours; the outer delete
        // removes every row (all device_codes / model_names) for those hours.
        let sql = format!(
            "DELETE FROM {table} WHERE hour_bucket IN (\
             SELECT DISTINCT hour_bucket FROM {table} WHERE hour_bucket < ? \
             ORDER BY hour_bucket ASC LIMIT {CLEANUP_BATCH_HOURS})"
        );
        let res = db
            .execute_raw(Statement::from_sql_and_values(
                backend,
                sql,
                vec![Value::from(cutoff)],
            ))
            .await?;
        total += res.rows_affected();
        // Rows strictly decrease each pass, so a zero-affected batch means done.
        if res.rows_affected() == 0 {
            break;
        }
    }
    Ok(total)
}

/// Recover lapsed active sessions, then delete settled sessions older than the
/// configured idle window together with their durable execution rows.
async fn cleanup_agent_sessions(
    db: &DatabaseConnection,
    cutoff: DateTimeUtc,
    now: DateTimeUtc,
) -> Result<(u64, u64), DbErr> {
    let store = crate::agent_session_store::SignalAgentSessionStore::new(db.clone());
    let mut settled = 0;
    for _ in 0..CLEANUP_MAX_BATCHES_PER_TICK {
        let rows = agent_session::Entity::find()
            .filter(agent_session::Column::LeaseDeadline.lt(now))
            .order_by_asc(agent_session::Column::LeaseDeadline)
            .limit(AGENT_SESSION_BATCH_ROWS)
            .all(db)
            .await?;
        if rows.is_empty() {
            break;
        }
        let mut advanced = false;
        for row in &rows {
            if store
                .settle_lapsed_session(row, now)
                .await
                .map_err(|error| DbErr::Custom(error.message))?
            {
                settled += 1;
                advanced = true;
            }
        }
        if rows.len() < AGENT_SESSION_BATCH_ROWS as usize || !advanced {
            break;
        }
    }

    let mut deleted = 0;
    for _ in 0..CLEANUP_MAX_BATCHES_PER_TICK {
        let rows = agent_session::Entity::find()
            .filter(agent_session::Column::UpdatedAt.lt(cutoff))
            .order_by_asc(agent_session::Column::UpdatedAt)
            .limit(AGENT_SESSION_BATCH_ROWS)
            .all(db)
            .await?;
        let ids: Vec<i64> = rows
            .iter()
            .filter_map(|row| {
                desk_diagnose_core::session::PersistedAgentSession::decode_json(&row.state_json)
                    .ok()
                    .filter(|session| !session.turn_state.is_active())
                    .map(|_| row.id)
            })
            .collect();
        if ids.is_empty() {
            break;
        }
        deleted += delete_expired_session_candidates(db, &ids, cutoff).await?;
        if rows.len() < AGENT_SESSION_BATCH_ROWS as usize {
            break;
        }
    }
    Ok((settled, deleted))
}

/// Recheck idle expiry in the same SQLite transaction that deletes all run data.
/// A concurrent turn update cannot be overwritten by upgrading a stale snapshot:
/// SQLite rejects that write, and the entire batch is rolled back.
async fn delete_expired_session_candidates(
    db: &DatabaseConnection,
    ids: &[i64],
    cutoff: DateTimeUtc,
) -> Result<u64, DbErr> {
    let txn = db.begin().await?;
    let result = async {
        let rows = agent_session::Entity::find()
            .filter(agent_session::Column::Id.is_in(ids.iter().copied()))
            .filter(agent_session::Column::UpdatedAt.lt(cutoff))
            .all(&txn)
            .await?;
        let rows: Vec<_> = rows
            .into_iter()
            .filter(|row| {
                desk_diagnose_core::session::PersistedAgentSession::decode_json(&row.state_json)
                    .is_ok_and(|session| !session.turn_state.is_active())
            })
            .collect();
        if rows.is_empty() {
            return Ok(0);
        }
        let run_ids: Vec<_> = rows.iter().map(|row| row.conversation_id.clone()).collect();
        let work_ids = agent_action_item::Entity::find()
            .select_only()
            .column(agent_action_item::Column::Id)
            .filter(agent_action_item::Column::ConversationId.is_in(run_ids.clone()))
            .into_query();
        agent_capability_dispatch_outbox::Entity::delete_many()
            .filter(agent_capability_dispatch_outbox::Column::WorkId.in_subquery(work_ids))
            .exec(&txn)
            .await?;
        agent_grant_reservation::Entity::delete_many()
            .filter(agent_grant_reservation::Column::RunId.is_in(run_ids.clone()))
            .exec(&txn)
            .await?;
        agent_capability_grant::Entity::delete_many()
            .filter(agent_capability_grant::Column::RunId.is_in(run_ids.clone()))
            .exec(&txn)
            .await?;
        agent_run_event::Entity::delete_many()
            .filter(agent_run_event::Column::RunId.is_in(run_ids.clone()))
            .exec(&txn)
            .await?;
        agent_action_item::Entity::delete_many()
            .filter(agent_action_item::Column::ConversationId.is_in(run_ids.clone()))
            .exec(&txn)
            .await?;
        agent_exec_task::Entity::delete_many()
            .filter(agent_exec_task::Column::ConversationId.is_in(run_ids))
            .exec(&txn)
            .await?;
        let deleted = agent_session::Entity::delete_many()
            .filter(agent_session::Column::Id.is_in(rows.iter().map(|row| row.id)))
            .exec(&txn)
            .await?;
        Ok(deleted.rows_affected)
    }
    .await;
    match result {
        Ok(deleted) => {
            txn.commit().await?;
            Ok(deleted)
        }
        Err(error) => {
            txn.rollback().await?;
            Err(error)
        }
    }
}

/// Run one cleanup pass over both rollup tables using the current retention config.
/// Returns `(turn_rows, ai_rows, settled_sessions, deleted_sessions)`.
pub async fn cleanup_once(
    db: &DatabaseConnection,
    now: DateTimeUtc,
) -> Result<(u64, u64, u64, u64), DbErr> {
    let cfg = load(db).await?;
    let turn = cleanup_table(
        db,
        TURN_USAGE_TABLE,
        now - chrono::Duration::days(cfg.turn_days as i64),
    )
    .await?;
    let ai = cleanup_table(
        db,
        AI_USAGE_TABLE,
        now - chrono::Duration::days(cfg.ai_days as i64),
    )
    .await?;
    let (settled, sessions) = cleanup_agent_sessions(
        db,
        now - chrono::Duration::days(cfg.agent_session_days as i64),
        now,
    )
    .await?;
    Ok((turn, ai, settled, sessions))
}

/// Run the retention cleanup forever on a fixed interval. Spawned once after the
/// signal DB is initialized (Default / Signaling / ServiceDaemon modes).
pub async fn run_retention_cleanup_loop(db: DatabaseConnection) {
    let mut ticker = tokio::time::interval(CLEANUP_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        match cleanup_once(&db, Utc::now()).await {
            Ok((t, a, settled, sessions)) if t > 0 || a > 0 || settled > 0 || sessions > 0 => {
                log::info!(
                    "Signal retention cleanup deleted {t} TURN + {a} AI rollup rows, \
                     settled {settled} lapsed and deleted {sessions} agent sessions"
                );
            }
            Ok(_) => {}
            Err(e) => log::warn!("Signal usage retention cleanup failed: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::{ConnectionTrait, Database, Schema};

    async fn memory_db() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        let schema = Schema::new(db.get_database_backend());
        let stmt = schema.create_table_from_entity(usage_retention::Entity);
        db.execute(&stmt).await.unwrap();
        db
    }

    #[tokio::test]
    async fn load_default_when_absent() {
        let db = memory_db().await;
        let cfg = load(&db).await.unwrap();
        assert_eq!(cfg, UsageRetentionConfig::default());
        assert_eq!(cfg.turn_days, DEFAULT_RETENTION_DAYS);
        assert_eq!(cfg.ai_days, DEFAULT_RETENTION_DAYS);
    }

    #[tokio::test]
    async fn save_then_load_round_trips() {
        let db = memory_db().await;
        save(
            &db,
            UsageRetentionConfig {
                turn_days: 90,
                ai_days: 45,
                agent_session_days: 60,
            },
        )
        .await
        .unwrap();
        let loaded = load(&db).await.unwrap();
        assert_eq!(loaded.turn_days, 90);
        assert_eq!(loaded.ai_days, 45);
        assert_eq!(loaded.agent_session_days, 60);
    }

    #[tokio::test]
    async fn save_is_last_write_wins_on_singleton_row() {
        let db = memory_db().await;
        save(
            &db,
            UsageRetentionConfig {
                turn_days: 30,
                ai_days: 30,
                agent_session_days: 30,
            },
        )
        .await
        .unwrap();
        save(
            &db,
            UsageRetentionConfig {
                turn_days: 7,
                ai_days: 7,
                agent_session_days: 14,
            },
        )
        .await
        .unwrap();
        // Still a single row, holding the latest write.
        let rows = usage_retention::Entity::find().all(&db).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].turn_days, 7);
        assert_eq!(rows[0].ai_days, 7);
        assert_eq!(rows[0].agent_session_days, 14);
    }

    #[test]
    fn validate_rejects_out_of_range() {
        assert!(
            UsageRetentionConfig {
                turn_days: 0,
                ai_days: 30,
                agent_session_days: 30,
            }
            .validate()
            .is_err()
        );
        assert!(
            UsageRetentionConfig {
                turn_days: 30,
                ai_days: MAX_RETENTION_DAYS + 1,
                agent_session_days: 30,
            }
            .validate()
            .is_err()
        );
        assert!(
            UsageRetentionConfig {
                turn_days: 1,
                ai_days: MAX_RETENTION_DAYS,
                agent_session_days: 30,
            }
            .validate()
            .is_ok()
        );
    }

    // ---- cleanup ----

    use crate::entity::{ai_usage, turn_usage};
    use sea_orm::ActiveModelTrait;

    async fn cleanup_db() -> DatabaseConnection {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        let schema = Schema::new(db.get_database_backend());
        for stmt in [
            schema.create_table_from_entity(usage_retention::Entity),
            schema.create_table_from_entity(turn_usage::Entity),
            schema.create_table_from_entity(ai_usage::Entity),
            schema.create_table_from_entity(agent_session::Entity),
            schema.create_table_from_entity(agent_exec_task::Entity),
            schema.create_table_from_entity(agent_action_item::Entity),
            schema.create_table_from_entity(agent_capability_dispatch_outbox::Entity),
            schema.create_table_from_entity(agent_capability_grant::Entity),
            schema.create_table_from_entity(agent_grant_reservation::Entity),
            schema.create_table_from_entity(agent_run_event::Entity),
        ] {
            db.execute(&stmt).await.unwrap();
        }
        db
    }

    fn now() -> DateTimeUtc {
        use chrono::TimeZone;
        Utc.with_ymd_and_hms(2026, 7, 8, 12, 0, 0).unwrap()
    }

    fn days_ago(d: i64) -> DateTimeUtc {
        now() - chrono::Duration::days(d)
    }

    async fn seed_turn(db: &DatabaseConnection, device: &str, hour: DateTimeUtc) {
        turn_usage::ActiveModel {
            device_code: Set(device.into()),
            hour_bucket: Set(hour),
            relay_received_bytes: Set(10),
            relay_sent_bytes: Set(20),
            relay_received_pkts: Set(1),
            relay_sent_pkts: Set(2),
            control_received_bytes: Set(3),
            control_sent_bytes: Set(4),
            control_received_pkts: Set(1),
            control_sent_pkts: Set(1),
            updated_at: Set(hour),
        }
        .insert(db)
        .await
        .unwrap();
    }

    async fn turn_hours(db: &DatabaseConnection) -> Vec<DateTimeUtc> {
        let mut hs: Vec<DateTimeUtc> = turn_usage::Entity::find()
            .all(db)
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.hour_bucket)
            .collect();
        hs.sort();
        hs
    }

    async fn seed_session(
        db: &DatabaseConnection,
        conversation_id: &str,
        updated_at: DateTimeUtc,
        active: bool,
        lease_deadline: Option<DateTimeUtc>,
    ) {
        use desk_agent_protocol::{AgentScope, ExecutionMode};
        use desk_diagnose_core::session::PersistedAgentSession;

        let mut session = PersistedAgentSession::new(
            conversation_id,
            "1",
            "device-1",
            0,
            AgentScope {
                granted: vec![],
                mode: ExecutionMode::ReadOnly,
                expires_at: None,
                policy_name: None,
            },
            updated_at.to_rfc3339(),
        );
        if active {
            session
                .begin_turn(
                    "turn-1",
                    Some("request-1".into()),
                    Some("browser-1".into()),
                    0,
                    session.scope_snapshot.clone(),
                    updated_at.to_rfc3339(),
                )
                .unwrap();
        }
        agent_session::ActiveModel {
            conversation_id: Set(conversation_id.into()),
            actor_id: Set("1".into()),
            device_id: Set("device-1".into()),
            state_json: Set(serde_json::to_string(&session).unwrap()),
            version: Set(0),
            lease_token: Set(session.lease_token as i64),
            lease_deadline: Set(lease_deadline),
            created_at: Set(updated_at),
            updated_at: Set(updated_at),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn cleanup_deletes_only_expired() {
        let db = cleanup_db().await;
        seed_turn(&db, "d1", days_ago(0)).await;
        seed_turn(&db, "d1", days_ago(40)).await;
        seed_turn(&db, "d1", days_ago(200)).await;
        // 30d retention.
        let deleted = cleanup_table(&db, TURN_USAGE_TABLE, days_ago(30))
            .await
            .unwrap();
        assert_eq!(deleted, 2);
        assert_eq!(turn_hours(&db).await, vec![days_ago(0)]);
    }

    #[tokio::test]
    async fn cleanup_is_idempotent() {
        let db = cleanup_db().await;
        seed_turn(&db, "d1", days_ago(200)).await;
        assert_eq!(
            cleanup_table(&db, TURN_USAGE_TABLE, days_ago(30))
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            cleanup_table(&db, TURN_USAGE_TABLE, days_ago(30))
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn cleanup_batches_more_than_one_pass() {
        let db = cleanup_db().await;
        for d in 40..40 + (CLEANUP_BATCH_HOURS as i64) + 5 {
            seed_turn(&db, "d1", days_ago(d)).await;
        }
        let deleted = cleanup_table(&db, TURN_USAGE_TABLE, days_ago(30))
            .await
            .unwrap();
        assert_eq!(deleted, CLEANUP_BATCH_HOURS + 5);
        assert!(turn_hours(&db).await.is_empty());
    }

    #[tokio::test]
    async fn agent_session_cleanup_deletes_only_old_settled_rows_and_recovers_lapsed() {
        let db = cleanup_db().await;
        seed_session(&db, "recent", days_ago(5), false, None).await;
        seed_session(&db, "old-settled", days_ago(40), false, None).await;
        seed_session(
            &db,
            "old-live",
            days_ago(40),
            true,
            Some(now() + chrono::Duration::hours(1)),
        )
        .await;
        seed_session(
            &db,
            "old-lapsed",
            days_ago(40),
            true,
            Some(now() - chrono::Duration::hours(1)),
        )
        .await;

        let (settled, deleted) = cleanup_agent_sessions(&db, days_ago(30), now())
            .await
            .unwrap();
        assert_eq!(settled, 1);
        assert_eq!(deleted, 1);
        let rows = agent_session::Entity::find().all(&db).await.unwrap();
        let ids: Vec<_> = rows
            .iter()
            .map(|row| row.conversation_id.as_str())
            .collect();
        assert_eq!(ids.len(), 3);
        assert!(ids.contains(&"recent"));
        assert!(ids.contains(&"old-live"));
        assert!(ids.contains(&"old-lapsed"));
        let recovered = rows
            .iter()
            .find(|row| row.conversation_id == "old-lapsed")
            .unwrap();
        let recovered: desk_diagnose_core::session::PersistedAgentSession =
            serde_json::from_str(&recovered.state_json).unwrap();
        assert!(!recovered.turn_state.is_active());
    }

    #[tokio::test]
    async fn cleanup_once_uses_config_windows() {
        let db = cleanup_db().await;
        // Default config = 30d for both.
        seed_turn(&db, "d1", days_ago(0)).await;
        seed_turn(&db, "d1", days_ago(400)).await;
        let (turn, ai, settled, sessions) = cleanup_once(&db, now()).await.unwrap();
        assert_eq!(turn, 1);
        assert_eq!(ai, 0);
        assert_eq!(settled, 0);
        assert_eq!(sessions, 0);
        assert_eq!(turn_hours(&db).await, vec![days_ago(0)]);
    }

    #[tokio::test]
    async fn stale_cleanup_candidates_cannot_delete_resumed_or_refreshed_sessions() {
        let db = cleanup_db().await;
        for run in ["resumed", "refreshed", "expired"] {
            seed_session(&db, run, days_ago(40), false, None).await;
        }
        let candidates = agent_session::Entity::find().all(&db).await.unwrap();
        for row in &candidates {
            let mut updated: agent_session::ActiveModel = row.clone().into();
            match row.conversation_id.as_str() {
                "resumed" => {
                    let mut session =
                        desk_diagnose_core::session::PersistedAgentSession::decode_json(
                            &row.state_json,
                        )
                        .unwrap();
                    session
                        .begin_turn(
                            "new-turn",
                            None,
                            None,
                            0,
                            session.scope_snapshot.clone(),
                            now().to_rfc3339(),
                        )
                        .unwrap();
                    updated.state_json = Set(session.encode_json_for_storage().unwrap());
                    updated.lease_token = Set(session.lease_token as i64);
                    updated.lease_deadline = Set(Some(now() + chrono::Duration::hours(1)));
                }
                "refreshed" => updated.updated_at = Set(now()),
                _ => continue,
            }
            updated.update(&db).await.unwrap();
        }
        let ids: Vec<_> = candidates.iter().map(|row| row.id).collect();
        assert_eq!(
            delete_expired_session_candidates(&db, &ids, days_ago(30))
                .await
                .unwrap(),
            1
        );
        let remaining = agent_session::Entity::find().all(&db).await.unwrap();
        assert_eq!(remaining.len(), 2);
        assert!(remaining.iter().all(|row| row.conversation_id != "expired"));
    }
}
