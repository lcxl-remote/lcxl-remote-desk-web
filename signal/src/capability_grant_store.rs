//! OSS SQLite CapabilityGrant issuance and atomic Prepare/DispatchIntent transactions.

use chrono::{TimeZone, Utc};
use desk_agent_protocol::capability_grant::{CAPABILITY_GRANT_SCHEMA_VERSION, CapabilityGrant};
use desk_diagnose_core::{
    capability_grant::{
        CapabilityGrantCall, match_capability_grant, match_reserved_capability_grant,
    },
    session::PersistedAgentSession,
};
use sea_orm::sea_query::Expr;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, DbErr, EntityTrait, QueryFilter, RuntimeErr,
    Set, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::entity::{
    agent_action_item, agent_capability_dispatch_outbox, agent_capability_grant,
    agent_grant_reservation, agent_session,
};

pub(crate) mod computer_background;
pub(crate) mod computer_binding;
pub(crate) mod computer_completion;
pub(crate) mod computer_delivery;
pub(crate) mod computer_export;

pub const GRANT_STATUS_ACTIVE: &str = "active";
pub const GRANT_STATUS_REVOKED: &str = "revoked";
pub const RESERVATION_STATUS_RESERVED: &str = "reserved";
pub const RESERVATION_STATUS_COMMITTED: &str = "committed";
pub const RESERVATION_STATUS_RELEASED: &str = "released";
pub const CAPABILITY_WORK_KIND: &str = "capability_provider";
pub const CAPABILITY_WORK_PREPARED: &str = "capability_prepared";
pub const CAPABILITY_WORK_INTENT_RECORDED: &str = "capability_intent_recorded";
pub const CAPABILITY_WORK_DISPATCHING: &str = "capability_dispatching";
pub const CAPABILITY_WORK_SUCCEEDED: &str = "capability_succeeded";
pub const CAPABILITY_WORK_FAILED: &str = "capability_failed";
pub const CAPABILITY_WORK_OUTCOME_UNKNOWN: &str = "capability_outcome_unknown";
pub const CAPABILITY_WORK_SUPERSEDED: &str = "capability_superseded_before_intent";
pub const CAPABILITY_WORK_REVOKED: &str = "capability_revoked_before_intent";
pub const DISPATCH_OUTBOX_PENDING: &str = "pending";
pub const DISPATCH_OUTBOX_SENDING: &str = "sending";
pub const DISPATCH_OUTBOX_COMPLETED: &str = "completed";
pub const DISPATCH_OUTBOX_OUTCOME_UNKNOWN: &str = "outcome_unknown";

const SQLITE_COMPLETION_BUSY_INITIAL_DELAY_MS: u64 = 5;
const SQLITE_COMPLETION_BUSY_MAX_DELAY_MS: u64 = 250;
const SQLITE_COMPLETION_BUSY_BUDGET_MS: u64 = 5_000;

fn retryable_sqlite_write_contention(error: &DbErr) -> bool {
    let runtime = match error {
        DbErr::Exec(runtime) | DbErr::Query(runtime) => runtime,
        _ => return false,
    };
    let RuntimeErr::SqlxError(error) = runtime else {
        return false;
    };
    let sea_orm::sqlx::Error::Database(database_error) = error.as_ref() else {
        return false;
    };
    database_error
        .code()
        .and_then(|code| code.parse::<i32>().ok())
        .is_some_and(|code| matches!(code & 0xff, 5 | 6))
}

#[cfg(test)]
fn pause_crash_fixture_before_commit(phase: &str) {
    const PHASE_ENV: &str = "DESK_SIGNAL_CAPABILITY_CRASH_PHASE";
    const MARKER_ENV: &str = "DESK_SIGNAL_CAPABILITY_CRASH_MARKER";
    if std::env::var(PHASE_ENV).ok().as_deref() != Some(phase) {
        return;
    }
    let marker = std::env::var(MARKER_ENV).expect("crash marker is configured");
    std::fs::write(marker, b"transaction-open").expect("write pre-commit crash marker");
    loop {
        std::thread::sleep(std::time::Duration::from_secs(60));
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreparedCapabilityPayload {
    pub grant_id: String,
    pub reservation_id: String,
    pub call_id: String,
    pub generation: u64,
    pub input_revision: u64,
    pub input_watermark: u64,
    pub canonical_input_json: String,
    pub canonical_input_digest_sha256: String,
    pub provider_id: String,
    pub capability_id: String,
    pub tool_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedCapabilityCall {
    pub work_id: i64,
    pub reservation_id: String,
    pub call_id: String,
    pub generation: u64,
    pub idempotent_replay: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityDispatchPayload {
    pub dispatch_id: String,
    pub grant_id: String,
    pub reservation_id: String,
    pub work_id: i64,
    pub call_id: String,
    pub generation: u64,
    pub input_revision: u64,
    pub input_watermark: u64,
    pub canonical_input_json: String,
    pub canonical_input_digest_sha256: String,
    pub provider_id: String,
    pub capability_id: String,
    pub tool_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchIntentResult {
    Recorded {
        dispatch_id: String,
        outbox_id: i64,
        idempotent_replay: bool,
    },
    SupersededBeforeIntent {
        work_id: i64,
        idempotent_replay: bool,
    },
    RevokedBeforeIntent {
        work_id: i64,
        idempotent_replay: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchClaimResult {
    Claimed(CapabilityDispatchPayload),
    OutcomeUnknown { dispatch_id: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityDispatchOutcome {
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityDispatchCompletion {
    pub dispatch_id: String,
    pub call_id: String,
    pub generation: u64,
    pub outcome: CapabilityDispatchOutcome,
    pub result_digest_sha256: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DispatchCompletionResult {
    pub work_id: i64,
    pub idempotent_replay: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DispatchUnknownResult {
    pub work_id: i64,
    pub idempotent_replay: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityManualDispositionResult {
    Applied,
    AlreadyResolved,
    SubjectMismatch,
    StateMismatch,
}

#[derive(Debug, Clone)]
pub struct PrepareCapabilityCall<'a> {
    pub grant_id: &'a str,
    pub call_id: &'a str,
    pub turn_id: &'a str,
    pub input_revision: u64,
    pub input_watermark: u64,
    pub generation: u64,
    pub canonical_input_json: &'a str,
    pub call: CapabilityGrantCall<'a>,
}

#[derive(Clone)]
pub struct SignalCapabilityGrantStore {
    db: DatabaseConnection,
}

impl SignalCapabilityGrantStore {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    pub async fn issue(
        &self,
        grant: &CapabilityGrant,
    ) -> Result<agent_capability_grant::Model, DbErr> {
        grant
            .validate()
            .map_err(|error| DbErr::Custom(format!("invalid capability grant: {error}")))?;
        if let Some(existing) = agent_capability_grant::Entity::find()
            .filter(agent_capability_grant::Column::GrantId.eq(&grant.grant_id))
            .one(&self.db)
            .await?
        {
            let stored = decode_grant(&existing)?;
            if &stored == grant {
                return Ok(existing);
            }
            return Err(DbErr::Custom(
                "grant id is already bound to different authority".into(),
            ));
        }
        agent_capability_grant::ActiveModel {
            grant_id: Set(grant.grant_id.clone()),
            actor_id: Set(grant.actor_id.clone()),
            run_id: Set(grant.run_id.clone()),
            provider_id: Set(grant.provider_id.clone()),
            tool_name: Set(grant.tool_name.clone()),
            status: Set(GRANT_STATUS_ACTIVE.into()),
            remaining_uses: Set(i32::try_from(grant.remaining_uses)
                .map_err(|_| DbErr::Custom("grant uses exceed SQLite range".into()))?),
            payload_json: Set(serde_json::to_string(grant).map_err(json_error)?),
            payload_schema_version: Set(i32::from(CAPABILITY_GRANT_SCHEMA_VERSION)),
            version: Set(1),
            created_at: Set(timestamp(grant.issued_at_unix_ms)?),
            updated_at: Set(timestamp(grant.issued_at_unix_ms)?),
            ..Default::default()
        }
        .insert(&self.db)
        .await
    }

    pub async fn list_for_subject(
        &self,
        run_id: &str,
        actor_id: &str,
        target_device_id: &str,
    ) -> Result<Vec<CapabilityGrant>, DbErr> {
        Self::list_for_subject_on(&self.db, run_id, actor_id, target_device_id).await
    }

    pub(crate) async fn list_for_subject_on<C: sea_orm::ConnectionTrait>(
        db: &C,
        run_id: &str,
        actor_id: &str,
        target_device_id: &str,
    ) -> Result<Vec<CapabilityGrant>, DbErr> {
        let rows = agent_capability_grant::Entity::find()
            .filter(agent_capability_grant::Column::RunId.eq(run_id))
            .filter(agent_capability_grant::Column::ActorId.eq(actor_id))
            .all(db)
            .await?;
        let mut grants = Vec::with_capacity(rows.len());
        for row in rows {
            let grant = decode_grant(&row)?;
            if grant.target_device_id == target_device_id {
                grants.push(grant);
            }
        }
        grants.sort_by(|left, right| left.grant_id.cmp(&right.grant_id));
        Ok(grants)
    }

    /// Return the grant already frozen into a Prepared-or-later call. This lets
    /// a safe pre-intent replay reuse the original reservation authority instead
    /// of minting a second policy-auto grant for the same stable call id.
    pub async fn prepared_grant_id(&self, call_id: &str) -> Result<Option<String>, DbErr> {
        let Some(work) = agent_action_item::Entity::find()
            .filter(agent_action_item::Column::Kind.eq(CAPABILITY_WORK_KIND))
            .filter(agent_action_item::Column::ActionRequestId.eq(call_id))
            .one(&self.db)
            .await?
        else {
            return Ok(None);
        };
        Ok(Some(decode_prepared_payload(&work)?.grant_id))
    }

    pub async fn revoke(
        &self,
        grant_id: &str,
        actor_id: &str,
        target_device_id: &str,
        now_unix_ms: u64,
        reason: &str,
    ) -> Result<CapabilityGrant, DbErr> {
        if reason.trim().is_empty() || reason.len() > 512 || reason.chars().any(char::is_control) {
            return Err(DbErr::Custom(
                "invalid capability grant revocation reason".into(),
            ));
        }
        let txn = self.db.begin().await?;
        let row = agent_capability_grant::Entity::find()
            .filter(agent_capability_grant::Column::GrantId.eq(grant_id))
            .one(&txn)
            .await?
            .ok_or_else(|| DbErr::Custom("capability grant was not found".into()))?;
        let mut grant = decode_grant(&row)?;
        if grant.actor_id != actor_id || grant.target_device_id != target_device_id {
            txn.rollback().await.ok();
            return Err(DbErr::Custom("capability grant subject mismatch".into()));
        }
        if grant.revoked_at_unix_ms.is_some() {
            txn.rollback().await.ok();
            return Ok(grant);
        }
        if now_unix_ms < grant.issued_at_unix_ms {
            txn.rollback().await.ok();
            return Err(DbErr::Custom("revocation predates grant issuance".into()));
        }
        grant.revoked_at_unix_ms = Some(now_unix_ms);
        grant.revoked_reason = Some(reason.trim().to_string());
        grant
            .validate()
            .map_err(|error| DbErr::Custom(format!("invalid revoked grant: {error}")))?;
        let now = timestamp(now_unix_ms)?;
        let updated = agent_capability_grant::Entity::update_many()
            .col_expr(
                agent_capability_grant::Column::Status,
                Expr::value(GRANT_STATUS_REVOKED),
            )
            .col_expr(
                agent_capability_grant::Column::PayloadJson,
                Expr::value(serde_json::to_string(&grant).map_err(json_error)?),
            )
            .col_expr(
                agent_capability_grant::Column::Version,
                Expr::value(row.version + 1),
            )
            .col_expr(agent_capability_grant::Column::UpdatedAt, Expr::value(now))
            .filter(agent_capability_grant::Column::Id.eq(row.id))
            .filter(agent_capability_grant::Column::Version.eq(row.version))
            .filter(agent_capability_grant::Column::Status.eq(GRANT_STATUS_ACTIVE))
            .exec(&txn)
            .await?;
        if updated.rows_affected != 1 {
            txn.rollback().await.ok();
            return Err(DbErr::Custom(
                "capability grant revocation conflicted".into(),
            ));
        }
        txn.commit().await?;
        Ok(grant)
    }

    /// Atomically reserve one use and create the ToolCall/work/generation facts.
    /// No outbox or dispatch intent exists after this method returns.
    pub async fn prepare(
        &self,
        request: PrepareCapabilityCall<'_>,
    ) -> Result<PreparedCapabilityCall, DbErr> {
        validate_prepare(&request)?;
        let txn = self.db.begin().await?;
        if let Some(existing) = load_prepared(&txn, request.call_id).await? {
            let prepared = validate_replay(&existing, &request)?;
            txn.rollback().await.ok();
            return Ok(prepared);
        }
        let grant_row = agent_capability_grant::Entity::find()
            .filter(agent_capability_grant::Column::GrantId.eq(request.grant_id))
            .one(&txn)
            .await?
            .ok_or_else(|| DbErr::Custom("capability grant was not found".into()))?;
        if grant_row.status != GRANT_STATUS_ACTIVE {
            txn.rollback().await.ok();
            return Err(DbErr::Custom("capability grant is not active".into()));
        }
        let mut grant = decode_grant(&grant_row)?;
        match_capability_grant(&grant, &request.call).map_err(|reason| {
            DbErr::Custom(format!("capability grant does not match call: {reason:?}"))
        })?;
        if grant.remaining_uses == 0 || grant_row.remaining_uses <= 0 {
            txn.rollback().await.ok();
            return Err(DbErr::Custom("capability grant is exhausted".into()));
        }
        let reservation_id = stable_id(
            "reservation",
            &format!("{}:{}", request.grant_id, request.call_id),
        );
        let payload = PreparedCapabilityPayload {
            grant_id: request.grant_id.to_string(),
            reservation_id: reservation_id.clone(),
            call_id: request.call_id.to_string(),
            generation: request.generation,
            input_revision: request.input_revision,
            input_watermark: request.input_watermark,
            canonical_input_json: request.canonical_input_json.to_string(),
            canonical_input_digest_sha256: request.call.canonical_input_digest_sha256.to_string(),
            provider_id: request.call.provider_id.to_string(),
            capability_id: request.call.capability_id.to_string(),
            tool_name: request.call.tool_name.to_string(),
        };
        let now = timestamp(request.call.now_unix_ms)?;
        let work = agent_action_item::ActiveModel {
            kind: Set(CAPABILITY_WORK_KIND.into()),
            action_request_id: Set(request.call_id.to_string()),
            exec_request_id: Set(None),
            conversation_id: Set(request.call.run_id.to_string()),
            turn_id: Set(request.turn_id.to_string()),
            tool_call_id: Set(request.call_id.to_string()),
            actor_id: Set(request.call.actor_id.to_string()),
            target_device_id: Set(request.call.target_device_id.to_string()),
            status: Set(CAPABILITY_WORK_PREPARED.into()),
            attempt: Set(0),
            execution_id: Set(Some(format!(
                "capability:{}:{}",
                request.call_id, request.generation
            ))),
            draft_hash: Set(request.call.canonical_input_digest_sha256.to_string()),
            policy_revision: Set(request.call.policy_revision),
            is_side_effecting: Set(request.call.effect.is_side_effecting()),
            payload_json: Set(serde_json::to_string(&payload).map_err(json_error)?),
            payload_schema_version: Set(1),
            completion_event_id: Set(stable_id("capability-completion", request.call_id)),
            completion_delivery_state: Set("pending".into()),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(&txn)
        .await?;
        agent_grant_reservation::ActiveModel {
            reservation_id: Set(reservation_id.clone()),
            grant_id: Set(request.grant_id.to_string()),
            run_id: Set(request.call.run_id.to_string()),
            call_id: Set(request.call_id.to_string()),
            work_id: Set(work.id),
            canonical_input_digest_sha256: Set(request
                .call
                .canonical_input_digest_sha256
                .to_string()),
            state: Set(RESERVATION_STATUS_RESERVED.into()),
            generation: Set(i64::try_from(request.generation)
                .map_err(|_| DbErr::Custom("generation exceeds SQLite range".into()))?),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(&txn)
        .await?;
        grant.remaining_uses -= 1;
        let remaining = i32::try_from(grant.remaining_uses)
            .map_err(|_| DbErr::Custom("grant uses exceed SQLite range".into()))?;
        let updated = agent_capability_grant::Entity::update_many()
            .col_expr(
                agent_capability_grant::Column::RemainingUses,
                Expr::value(remaining),
            )
            .col_expr(
                agent_capability_grant::Column::PayloadJson,
                Expr::value(serde_json::to_string(&grant).map_err(json_error)?),
            )
            .col_expr(
                agent_capability_grant::Column::Version,
                Expr::value(grant_row.version + 1),
            )
            .col_expr(agent_capability_grant::Column::UpdatedAt, Expr::value(now))
            .filter(agent_capability_grant::Column::Id.eq(grant_row.id))
            .filter(agent_capability_grant::Column::Version.eq(grant_row.version))
            .filter(agent_capability_grant::Column::RemainingUses.eq(grant_row.remaining_uses))
            .exec(&txn)
            .await?;
        if updated.rows_affected != 1 {
            txn.rollback().await.ok();
            return Err(DbErr::Custom(
                "capability grant reservation conflicted".into(),
            ));
        }
        #[cfg(test)]
        pause_crash_fixture_before_commit("prepare_before_commit");
        txn.commit().await?;
        Ok(PreparedCapabilityCall {
            work_id: work.id,
            reservation_id,
            call_id: request.call_id.to_string(),
            generation: request.generation,
            idempotent_replay: false,
        })
    }

    /// Atomically freeze a provider dispatch intent with its exact input bytes.
    ///
    /// A newer durably accepted user input wins before this transaction: the
    /// reservation is released and the use is restored. After the outbox row is
    /// committed the use is never restored automatically, including after a
    /// crash, because external execution may already have happened.
    pub async fn record_dispatch_intent(
        &self,
        request: PrepareCapabilityCall<'_>,
    ) -> Result<DispatchIntentResult, DbErr> {
        validate_prepare(&request)?;
        let txn = self.db.begin().await?;
        let existing = load_prepared(&txn, request.call_id)
            .await?
            .ok_or_else(|| DbErr::Custom("capability call was not prepared".into()))?;
        validate_replay(&existing, &request)?;
        let (reservation, work) = existing;

        if reservation.state == RESERVATION_STATUS_RELEASED
            && work.status == CAPABILITY_WORK_SUPERSEDED
        {
            txn.rollback().await.ok();
            return Ok(DispatchIntentResult::SupersededBeforeIntent {
                work_id: work.id,
                idempotent_replay: true,
            });
        }
        if reservation.state == RESERVATION_STATUS_RELEASED
            && work.status == CAPABILITY_WORK_REVOKED
        {
            txn.rollback().await.ok();
            return Ok(DispatchIntentResult::RevokedBeforeIntent {
                work_id: work.id,
                idempotent_replay: true,
            });
        }
        if let Some(outbox) = agent_capability_dispatch_outbox::Entity::find()
            .filter(agent_capability_dispatch_outbox::Column::CallId.eq(request.call_id))
            .one(&txn)
            .await?
        {
            validate_outbox_replay(&outbox, &reservation, &work, &request)?;
            txn.rollback().await.ok();
            return Ok(DispatchIntentResult::Recorded {
                dispatch_id: outbox.dispatch_id,
                outbox_id: outbox.id,
                idempotent_replay: true,
            });
        }
        if reservation.state != RESERVATION_STATUS_RESERVED
            || work.status != CAPABILITY_WORK_PREPARED
        {
            txn.rollback().await.ok();
            return Err(DbErr::Custom(
                "prepared capability call has an invalid pre-intent state".into(),
            ));
        }

        let session_row = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(request.call.run_id))
            .one(&txn)
            .await?
            .ok_or_else(|| DbErr::Custom("authoritative agent session was not found".into()))?;
        let session =
            PersistedAgentSession::decode_json(&session_row.state_json).map_err(|error| {
                DbErr::Custom(format!("invalid authoritative agent session: {error}"))
            })?;
        if session.actor_id != request.call.actor_id
            || session.device_id != request.call.target_device_id
            || session_row.actor_id != request.call.actor_id
            || session_row.device_id != request.call.target_device_id
        {
            txn.rollback().await.ok();
            return Err(DbErr::Custom(
                "capability call no longer belongs to the authoritative session subject".into(),
            ));
        }
        let prepared_payload = decode_prepared_payload(&work)?;
        if session.input_revision != prepared_payload.input_revision
            || session.latest_input_seq != prepared_payload.input_watermark
        {
            release_before_intent(
                &txn,
                &reservation,
                &work,
                request.call.now_unix_ms,
                CAPABILITY_WORK_SUPERSEDED,
                "newer_user_input_before_dispatch_intent",
            )
            .await?;
            txn.commit().await?;
            return Ok(DispatchIntentResult::SupersededBeforeIntent {
                work_id: work.id,
                idempotent_replay: false,
            });
        }

        let grant_row = agent_capability_grant::Entity::find()
            .filter(agent_capability_grant::Column::GrantId.eq(request.grant_id))
            .one(&txn)
            .await?
            .ok_or_else(|| DbErr::Custom("capability grant was not found".into()))?;
        if grant_row.status != GRANT_STATUS_ACTIVE {
            release_before_intent(
                &txn,
                &reservation,
                &work,
                request.call.now_unix_ms,
                CAPABILITY_WORK_REVOKED,
                "grant_revoked_before_dispatch_intent",
            )
            .await?;
            txn.commit().await?;
            return Ok(DispatchIntentResult::RevokedBeforeIntent {
                work_id: work.id,
                idempotent_replay: false,
            });
        }
        let grant = decode_grant(&grant_row)?;
        match_reserved_capability_grant(&grant, &request.call).map_err(|reason| {
            DbErr::Custom(format!(
                "reserved capability grant no longer matches call: {reason:?}"
            ))
        })?;

        let dispatch_id = stable_id(
            "capability-dispatch",
            &format!("{}:{}", request.call_id, request.generation),
        );
        let dispatch_payload = CapabilityDispatchPayload {
            dispatch_id: dispatch_id.clone(),
            grant_id: request.grant_id.to_string(),
            reservation_id: reservation.reservation_id.clone(),
            work_id: work.id,
            call_id: request.call_id.to_string(),
            generation: request.generation,
            input_revision: prepared_payload.input_revision,
            input_watermark: prepared_payload.input_watermark,
            canonical_input_json: prepared_payload.canonical_input_json,
            canonical_input_digest_sha256: prepared_payload.canonical_input_digest_sha256,
            provider_id: prepared_payload.provider_id,
            capability_id: prepared_payload.capability_id,
            tool_name: prepared_payload.tool_name,
        };
        let now = timestamp(request.call.now_unix_ms)?;
        let outbox = agent_capability_dispatch_outbox::ActiveModel {
            dispatch_id: Set(dispatch_id.clone()),
            call_id: Set(request.call_id.to_string()),
            work_id: Set(work.id),
            reservation_id: Set(reservation.reservation_id.clone()),
            generation: Set(i64::try_from(request.generation)
                .map_err(|_| DbErr::Custom("generation exceeds SQLite range".into()))?),
            state: Set(DISPATCH_OUTBOX_PENDING.into()),
            payload_json: Set(serde_json::to_string(&dispatch_payload).map_err(json_error)?),
            payload_schema_version: Set(1),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(&txn)
        .await?;
        let reserved = agent_grant_reservation::Entity::update_many()
            .col_expr(
                agent_grant_reservation::Column::State,
                Expr::value(RESERVATION_STATUS_COMMITTED),
            )
            .col_expr(agent_grant_reservation::Column::UpdatedAt, Expr::value(now))
            .filter(agent_grant_reservation::Column::Id.eq(reservation.id))
            .filter(agent_grant_reservation::Column::State.eq(RESERVATION_STATUS_RESERVED))
            .exec(&txn)
            .await?;
        let intended = agent_action_item::Entity::update_many()
            .col_expr(
                agent_action_item::Column::Status,
                Expr::value(CAPABILITY_WORK_INTENT_RECORDED),
            )
            .col_expr(agent_action_item::Column::DispatchedAttempt, Expr::value(1))
            .col_expr(
                agent_action_item::Column::DispatchIntentAt,
                Expr::value(now),
            )
            .col_expr(agent_action_item::Column::UpdatedAt, Expr::value(now))
            .filter(agent_action_item::Column::Id.eq(work.id))
            .filter(agent_action_item::Column::Status.eq(CAPABILITY_WORK_PREPARED))
            .exec(&txn)
            .await?;
        if reserved.rows_affected != 1 || intended.rows_affected != 1 {
            txn.rollback().await.ok();
            return Err(DbErr::Custom(
                "capability dispatch intent conflicted".into(),
            ));
        }
        #[cfg(test)]
        pause_crash_fixture_before_commit("intent_before_commit");
        txn.commit().await?;
        Ok(DispatchIntentResult::Recorded {
            dispatch_id,
            outbox_id: outbox.id,
            idempotent_replay: false,
        })
    }

    /// Claim an intent created by this live process for one external handoff.
    /// The `sending` state is durable before bytes leave the process. Callers
    /// must never call this twice for the same dispatch in one process.
    pub async fn claim_dispatch(
        &self,
        dispatch_id: &str,
        now_unix_ms: u64,
    ) -> Result<DispatchClaimResult, DbErr> {
        let txn = self.db.begin().await?;
        let outbox = agent_capability_dispatch_outbox::Entity::find()
            .filter(agent_capability_dispatch_outbox::Column::DispatchId.eq(dispatch_id))
            .one(&txn)
            .await?
            .ok_or_else(|| DbErr::Custom("capability dispatch was not found".into()))?;
        if outbox.state == DISPATCH_OUTBOX_OUTCOME_UNKNOWN {
            txn.rollback().await.ok();
            return Ok(DispatchClaimResult::OutcomeUnknown {
                dispatch_id: dispatch_id.to_string(),
            });
        }
        if outbox.state != DISPATCH_OUTBOX_PENDING {
            txn.rollback().await.ok();
            return Err(DbErr::Custom(
                "capability dispatch is already claimed; automatic retry is forbidden".into(),
            ));
        }
        let payload: CapabilityDispatchPayload =
            serde_json::from_str(&outbox.payload_json).map_err(json_error)?;
        if payload.dispatch_id != dispatch_id || payload.work_id != outbox.work_id {
            txn.rollback().await.ok();
            return Err(DbErr::Custom(
                "capability dispatch outbox payload disagrees with its authority row".into(),
            ));
        }
        let now = timestamp(now_unix_ms)?;
        let claimed = agent_capability_dispatch_outbox::Entity::update_many()
            .col_expr(
                agent_capability_dispatch_outbox::Column::State,
                Expr::value(DISPATCH_OUTBOX_SENDING),
            )
            .col_expr(
                agent_capability_dispatch_outbox::Column::UpdatedAt,
                Expr::value(now),
            )
            .filter(agent_capability_dispatch_outbox::Column::Id.eq(outbox.id))
            .filter(agent_capability_dispatch_outbox::Column::State.eq(DISPATCH_OUTBOX_PENDING))
            .exec(&txn)
            .await?;
        let dispatching = agent_action_item::Entity::update_many()
            .col_expr(
                agent_action_item::Column::Status,
                Expr::value(CAPABILITY_WORK_DISPATCHING),
            )
            .col_expr(agent_action_item::Column::UpdatedAt, Expr::value(now))
            .filter(agent_action_item::Column::Id.eq(outbox.work_id))
            .filter(agent_action_item::Column::Status.eq(CAPABILITY_WORK_INTENT_RECORDED))
            .exec(&txn)
            .await?;
        if claimed.rows_affected != 1 || dispatching.rows_affected != 1 {
            txn.rollback().await.ok();
            return Err(DbErr::Custom("capability dispatch claim conflicted".into()));
        }
        txn.commit().await?;
        Ok(DispatchClaimResult::Claimed(payload))
    }

    /// Converge a claimed dispatch whose terminal result cannot be proven.
    /// The committed reservation is deliberately retained and no retry is
    /// scheduled: once the handoff boundary was crossed, absence of a result is
    /// not evidence that the Provider did not run.
    pub async fn mark_dispatch_outcome_unknown(
        &self,
        dispatch_id: &str,
        call_id: &str,
        generation: u64,
        now_unix_ms: u64,
    ) -> Result<DispatchUnknownResult, DbErr> {
        let deadline = tokio::time::Instant::now()
            + std::time::Duration::from_millis(SQLITE_COMPLETION_BUSY_BUDGET_MS);
        let mut delay = SQLITE_COMPLETION_BUSY_INITIAL_DELAY_MS;
        loop {
            match self
                .mark_dispatch_outcome_unknown_once(dispatch_id, call_id, generation, now_unix_ms)
                .await
            {
                Err(error)
                    if retryable_sqlite_write_contention(&error)
                        && tokio::time::Instant::now() < deadline =>
                {
                    tokio::time::sleep(
                        std::time::Duration::from_millis(delay)
                            .min(deadline.saturating_duration_since(tokio::time::Instant::now())),
                    )
                    .await;
                    delay = delay
                        .saturating_mul(2)
                        .min(SQLITE_COMPLETION_BUSY_MAX_DELAY_MS);
                }
                result => return result,
            }
        }
    }

    async fn mark_dispatch_outcome_unknown_once(
        &self,
        dispatch_id: &str,
        call_id: &str,
        generation: u64,
        now_unix_ms: u64,
    ) -> Result<DispatchUnknownResult, DbErr> {
        let txn = self.db.begin().await?;
        let outbox = agent_capability_dispatch_outbox::Entity::find()
            .filter(agent_capability_dispatch_outbox::Column::DispatchId.eq(dispatch_id))
            .one(&txn)
            .await?
            .ok_or_else(|| DbErr::Custom("capability dispatch was not found".into()))?;
        let payload: CapabilityDispatchPayload =
            serde_json::from_str(&outbox.payload_json).map_err(json_error)?;
        if payload.dispatch_id != dispatch_id
            || payload.call_id != call_id
            || payload.generation != generation
            || outbox.call_id != call_id
            || u64::try_from(outbox.generation).ok() != Some(generation)
        {
            txn.rollback().await.ok();
            return Err(DbErr::Custom(
                "capability unknown outcome disagrees with dispatch authority".into(),
            ));
        }
        let work = agent_action_item::Entity::find_by_id(outbox.work_id)
            .one(&txn)
            .await?
            .ok_or_else(|| DbErr::Custom("capability dispatch work was not found".into()))?;
        if outbox.state == DISPATCH_OUTBOX_OUTCOME_UNKNOWN
            && work.status == CAPABILITY_WORK_OUTCOME_UNKNOWN
        {
            txn.rollback().await.ok();
            return Ok(DispatchUnknownResult {
                work_id: work.id,
                idempotent_replay: true,
            });
        }
        if outbox.state == DISPATCH_OUTBOX_COMPLETED && work.result_schema_version == Some(2) {
            // A genuine completion may win the foreground timeout race. Keep
            // that terminal receipt; the caller reads it after this transition.
            let original = computer_completion::terminal_result(&outbox, work, &payload)?
                .ok_or_else(|| DbErr::Custom("missing original terminal receipt".into()))?;
            txn.rollback().await?;
            return Ok(DispatchUnknownResult {
                work_id: original.work.id,
                idempotent_replay: true,
            });
        }
        if outbox.state != DISPATCH_OUTBOX_SENDING || work.status != CAPABILITY_WORK_DISPATCHING {
            txn.rollback().await.ok();
            return Err(DbErr::Custom(
                "only a claimed capability dispatch may become outcome unknown".into(),
            ));
        }
        let now = timestamp(now_unix_ms)?;
        let unknown_outbox = agent_capability_dispatch_outbox::Entity::update_many()
            .col_expr(
                agent_capability_dispatch_outbox::Column::State,
                Expr::value(DISPATCH_OUTBOX_OUTCOME_UNKNOWN),
            )
            .col_expr(
                agent_capability_dispatch_outbox::Column::UpdatedAt,
                Expr::value(now),
            )
            .filter(agent_capability_dispatch_outbox::Column::Id.eq(outbox.id))
            .filter(agent_capability_dispatch_outbox::Column::State.eq(DISPATCH_OUTBOX_SENDING))
            .exec(&txn)
            .await?;
        let unknown_work = agent_action_item::Entity::update_many()
            .col_expr(
                agent_action_item::Column::Status,
                Expr::value(CAPABILITY_WORK_OUTCOME_UNKNOWN),
            )
            .col_expr(
                agent_action_item::Column::Resolution,
                Expr::value("provider_result_not_proven"),
            )
            .col_expr(agent_action_item::Column::UpdatedAt, Expr::value(now))
            .filter(agent_action_item::Column::Id.eq(work.id))
            .filter(agent_action_item::Column::Status.eq(CAPABILITY_WORK_DISPATCHING))
            .exec(&txn)
            .await?;
        if unknown_outbox.rows_affected != 1 || unknown_work.rows_affected != 1 {
            txn.rollback().await.ok();
            return Err(DbErr::Custom(
                "capability unknown outcome transition conflicted".into(),
            ));
        }
        txn.commit().await?;
        Ok(DispatchUnknownResult {
            work_id: work.id,
            idempotent_replay: false,
        })
    }

    /// Record an owner disposition for the exact capability dispatch whose
    /// durable outcome is unknown. The outbox and work remain unknown so the
    /// audit trail is truthful; the committed reservation is not restored.
    pub async fn manually_dispose_unknown_for_subject(
        &self,
        work_id: i64,
        dispatch_id: &str,
        conversation_id: &str,
        actor_id: &str,
        target_device_id: &str,
        now_unix_ms: u64,
    ) -> Result<CapabilityManualDispositionResult, DbErr> {
        let txn = self.db.begin().await?;
        let Some(outbox) = agent_capability_dispatch_outbox::Entity::find()
            .filter(agent_capability_dispatch_outbox::Column::DispatchId.eq(dispatch_id))
            .filter(agent_capability_dispatch_outbox::Column::WorkId.eq(work_id))
            .one(&txn)
            .await?
        else {
            txn.rollback().await.ok();
            return Ok(CapabilityManualDispositionResult::SubjectMismatch);
        };
        let payload: CapabilityDispatchPayload =
            serde_json::from_str(&outbox.payload_json).map_err(json_error)?;
        let Some(work) = agent_action_item::Entity::find_by_id(work_id)
            .one(&txn)
            .await?
        else {
            txn.rollback().await.ok();
            return Ok(CapabilityManualDispositionResult::SubjectMismatch);
        };
        if payload.dispatch_id != dispatch_id
            || payload.work_id != work_id
            || work.conversation_id != conversation_id
            || work.actor_id != actor_id
            || work.target_device_id != target_device_id
        {
            txn.rollback().await.ok();
            return Ok(CapabilityManualDispositionResult::SubjectMismatch);
        }
        if work.manual_resolved_at.is_some() {
            txn.rollback().await.ok();
            return Ok(CapabilityManualDispositionResult::AlreadyResolved);
        }
        if outbox.state != DISPATCH_OUTBOX_OUTCOME_UNKNOWN
            || work.status != CAPABILITY_WORK_OUTCOME_UNKNOWN
        {
            txn.rollback().await.ok();
            return Ok(CapabilityManualDispositionResult::StateMismatch);
        }
        let now = timestamp(now_unix_ms)?;
        let updated = agent_action_item::Entity::update_many()
            .col_expr(
                agent_action_item::Column::ManualResolvedAt,
                Expr::value(Some(now)),
            )
            .col_expr(agent_action_item::Column::UpdatedAt, Expr::value(now))
            .filter(agent_action_item::Column::Id.eq(work_id))
            .filter(agent_action_item::Column::Status.eq(CAPABILITY_WORK_OUTCOME_UNKNOWN))
            .filter(agent_action_item::Column::ManualResolvedAt.is_null())
            .exec(&txn)
            .await?;
        if updated.rows_affected == 1 {
            txn.commit().await?;
            Ok(CapabilityManualDispositionResult::Applied)
        } else {
            txn.rollback().await.ok();
            Ok(CapabilityManualDispositionResult::AlreadyResolved)
        }
    }

    /// Persist a bounded terminal Provider fact. A completion may arrive after
    /// startup recovery marked the dispatch unknown; that late fact may refine
    /// the outcome, but it never restores the consumed grant or causes retry.
    pub async fn record_dispatch_completion(
        &self,
        completion: &CapabilityDispatchCompletion,
        now_unix_ms: u64,
    ) -> Result<DispatchCompletionResult, DbErr> {
        validate_completion(completion)?;
        let mut delay_ms = SQLITE_COMPLETION_BUSY_INITIAL_DELAY_MS;
        let deadline = tokio::time::Instant::now()
            + std::time::Duration::from_millis(SQLITE_COMPLETION_BUSY_BUDGET_MS);
        loop {
            match self
                .record_dispatch_completion_once(completion, now_unix_ms)
                .await
            {
                Ok(result) => return Ok(result),
                Err(error) if retryable_sqlite_write_contention(&error) => {
                    // This retries only the idempotent database fact keyed by
                    // dispatch_id/call_id/generation. The Provider action is
                    // never called again. SQLite can reject a deferred
                    // read-to-write transaction upgrade immediately when a
                    // concurrent completion owns the WAL writer; beginning a
                    // fresh transaction after a bounded delay is the safe
                    // recovery path.
                    let now = tokio::time::Instant::now();
                    if now >= deadline {
                        return Err(error);
                    }
                    let remaining = deadline.saturating_duration_since(now);
                    let delay = std::time::Duration::from_millis(delay_ms).min(remaining);
                    tokio::time::sleep(delay).await;
                    if tokio::time::Instant::now() >= deadline {
                        return Err(error);
                    }
                    delay_ms = delay_ms
                        .saturating_mul(2)
                        .min(SQLITE_COMPLETION_BUSY_MAX_DELAY_MS);
                }
                Err(error) => return Err(error),
            }
        }
    }

    async fn record_dispatch_completion_once(
        &self,
        completion: &CapabilityDispatchCompletion,
        now_unix_ms: u64,
    ) -> Result<DispatchCompletionResult, DbErr> {
        let txn = self.db.begin().await?;
        let result = async {
            let outbox = agent_capability_dispatch_outbox::Entity::find()
                .filter(
                    agent_capability_dispatch_outbox::Column::DispatchId
                        .eq(&completion.dispatch_id),
                )
                .one(&txn)
                .await?
                .ok_or_else(|| DbErr::Custom("capability dispatch was not found".into()))?;
            let payload: CapabilityDispatchPayload =
                serde_json::from_str(&outbox.payload_json).map_err(json_error)?;
            if payload.dispatch_id != completion.dispatch_id
                || payload.call_id != completion.call_id
                || payload.generation != completion.generation
                || outbox.call_id != completion.call_id
                || u64::try_from(outbox.generation).ok() != Some(completion.generation)
            {
                return Err(DbErr::Custom(
                    "capability completion disagrees with dispatch authority".into(),
                ));
            }
            let work = agent_action_item::Entity::find_by_id(outbox.work_id)
                .one(&txn)
                .await?
                .ok_or_else(|| DbErr::Custom("capability dispatch work was not found".into()))?;
            let expected_work_status = match completion.outcome {
                CapabilityDispatchOutcome::Succeeded => CAPABILITY_WORK_SUCCEEDED,
                CapabilityDispatchOutcome::Failed => CAPABILITY_WORK_FAILED,
            };
            let result_json = serde_json::to_string(completion).map_err(json_error)?;
            if outbox.state == DISPATCH_OUTBOX_COMPLETED {
                if work.status == expected_work_status
                    && work.result_json.as_deref() == Some(result_json.as_str())
                    && work.result_schema_version == Some(1)
                {
                    return Ok(DispatchCompletionResult {
                        work_id: work.id,
                        idempotent_replay: true,
                    });
                }
                return Err(DbErr::Custom(
                    "capability dispatch completion conflicts with the terminal fact".into(),
                ));
            }
            if !matches!(
                outbox.state.as_str(),
                DISPATCH_OUTBOX_SENDING | DISPATCH_OUTBOX_OUTCOME_UNKNOWN
            ) || !matches!(
                work.status.as_str(),
                CAPABILITY_WORK_DISPATCHING | CAPABILITY_WORK_OUTCOME_UNKNOWN
            ) {
                return Err(DbErr::Custom(
                    "capability dispatch was not handed off; completion is not admissible".into(),
                ));
            }
            let now = timestamp(now_unix_ms)?;
            let completed_outbox = agent_capability_dispatch_outbox::Entity::update_many()
                .col_expr(
                    agent_capability_dispatch_outbox::Column::State,
                    Expr::value(DISPATCH_OUTBOX_COMPLETED),
                )
                .col_expr(
                    agent_capability_dispatch_outbox::Column::UpdatedAt,
                    Expr::value(now),
                )
                .filter(agent_capability_dispatch_outbox::Column::Id.eq(outbox.id))
                .filter(
                    agent_capability_dispatch_outbox::Column::State
                        .is_in([DISPATCH_OUTBOX_SENDING, DISPATCH_OUTBOX_OUTCOME_UNKNOWN]),
                )
                .exec(&txn)
                .await?;
            let completed_work = agent_action_item::Entity::update_many()
                .col_expr(
                    agent_action_item::Column::Status,
                    Expr::value(expected_work_status),
                )
                .col_expr(
                    agent_action_item::Column::ResultJson,
                    Expr::value(result_json),
                )
                .col_expr(
                    agent_action_item::Column::ResultSchemaVersion,
                    Expr::value(1),
                )
                .col_expr(
                    agent_action_item::Column::Resolution,
                    Expr::value(match completion.outcome {
                        CapabilityDispatchOutcome::Succeeded => "provider_succeeded",
                        CapabilityDispatchOutcome::Failed => "provider_failed",
                    }),
                )
                .col_expr(agent_action_item::Column::UpdatedAt, Expr::value(now))
                .filter(agent_action_item::Column::Id.eq(work.id))
                .filter(
                    agent_action_item::Column::Status
                        .is_in([CAPABILITY_WORK_DISPATCHING, CAPABILITY_WORK_OUTCOME_UNKNOWN]),
                )
                .exec(&txn)
                .await?;
            if completed_outbox.rows_affected != 1 || completed_work.rows_affected != 1 {
                return Err(DbErr::Custom(
                    "capability dispatch completion conflicted".into(),
                ));
            }
            Ok(DispatchCompletionResult {
                work_id: work.id,
                idempotent_replay: false,
            })
        }
        .await;
        match result {
            Ok(result) => {
                txn.commit().await?;
                Ok(result)
            }
            Err(error) => {
                // Do not rely on the asynchronous transaction Drop path here:
                // the next bounded retry must start only after this attempt has
                // definitely released every SQLite read/write lock.
                txn.rollback().await.ok();
                Err(error)
            }
        }
    }

    /// Startup fence. It must run before a dispatcher starts claiming new work.
    /// Every intent left pending/sending by the previous process is uncertain:
    /// preserve the consumed grant and require explicit reconciliation.
    pub async fn recover_unfinished_dispatches_after_restart(
        &self,
        now_unix_ms: u64,
    ) -> Result<u64, DbErr> {
        let txn = self.db.begin().await?;
        let rows = agent_capability_dispatch_outbox::Entity::find()
            .filter(
                agent_capability_dispatch_outbox::Column::State
                    .is_in([DISPATCH_OUTBOX_PENDING, DISPATCH_OUTBOX_SENDING]),
            )
            .all(&txn)
            .await?;
        let now = timestamp(now_unix_ms)?;
        let mut recovered = 0_u64;
        for outbox in rows {
            let outbox_update = agent_capability_dispatch_outbox::Entity::update_many()
                .col_expr(
                    agent_capability_dispatch_outbox::Column::State,
                    Expr::value(DISPATCH_OUTBOX_OUTCOME_UNKNOWN),
                )
                .col_expr(
                    agent_capability_dispatch_outbox::Column::UpdatedAt,
                    Expr::value(now),
                )
                .filter(agent_capability_dispatch_outbox::Column::Id.eq(outbox.id))
                .filter(
                    agent_capability_dispatch_outbox::Column::State
                        .is_in([DISPATCH_OUTBOX_PENDING, DISPATCH_OUTBOX_SENDING]),
                )
                .exec(&txn)
                .await?;
            let work_update = agent_action_item::Entity::update_many()
                .col_expr(
                    agent_action_item::Column::Status,
                    Expr::value(CAPABILITY_WORK_OUTCOME_UNKNOWN),
                )
                .col_expr(
                    agent_action_item::Column::Resolution,
                    Expr::value("restart_after_dispatch_intent"),
                )
                .col_expr(agent_action_item::Column::UpdatedAt, Expr::value(now))
                .filter(agent_action_item::Column::Id.eq(outbox.work_id))
                .filter(
                    agent_action_item::Column::Status
                        .is_in([CAPABILITY_WORK_INTENT_RECORDED, CAPABILITY_WORK_DISPATCHING]),
                )
                .exec(&txn)
                .await?;
            if outbox_update.rows_affected != 1 || work_update.rows_affected != 1 {
                txn.rollback().await.ok();
                return Err(DbErr::Custom(
                    "capability restart recovery conflicted".into(),
                ));
            }
            recovered += 1;
        }
        txn.commit().await?;
        Ok(recovered)
    }
}

async fn release_before_intent<C: sea_orm::ConnectionTrait>(
    db: &C,
    reservation: &agent_grant_reservation::Model,
    work: &agent_action_item::Model,
    now_unix_ms: u64,
    work_status: &str,
    resolution: &str,
) -> Result<(), DbErr> {
    let grant_row = agent_capability_grant::Entity::find()
        .filter(agent_capability_grant::Column::GrantId.eq(&reservation.grant_id))
        .one(db)
        .await?
        .ok_or_else(|| DbErr::Custom("reserved capability grant was not found".into()))?;
    let mut grant = decode_grant(&grant_row)?;
    if grant.remaining_uses >= grant.limits.max_calls {
        return Err(DbErr::Custom(
            "released grant use would exceed its issued limit".into(),
        ));
    }
    grant.remaining_uses += 1;
    let now = timestamp(now_unix_ms)?;
    let released = agent_grant_reservation::Entity::update_many()
        .col_expr(
            agent_grant_reservation::Column::State,
            Expr::value(RESERVATION_STATUS_RELEASED),
        )
        .col_expr(agent_grant_reservation::Column::UpdatedAt, Expr::value(now))
        .filter(agent_grant_reservation::Column::Id.eq(reservation.id))
        .filter(agent_grant_reservation::Column::State.eq(RESERVATION_STATUS_RESERVED))
        .exec(db)
        .await?;
    let superseded = agent_action_item::Entity::update_many()
        .col_expr(agent_action_item::Column::Status, Expr::value(work_status))
        .col_expr(
            agent_action_item::Column::Resolution,
            Expr::value(resolution),
        )
        .col_expr(agent_action_item::Column::UpdatedAt, Expr::value(now))
        .filter(agent_action_item::Column::Id.eq(work.id))
        .filter(agent_action_item::Column::Status.eq(CAPABILITY_WORK_PREPARED))
        .exec(db)
        .await?;
    let restored = agent_capability_grant::Entity::update_many()
        .col_expr(
            agent_capability_grant::Column::RemainingUses,
            Expr::value(
                i32::try_from(grant.remaining_uses)
                    .map_err(|_| DbErr::Custom("grant uses exceed SQLite range".into()))?,
            ),
        )
        .col_expr(
            agent_capability_grant::Column::PayloadJson,
            Expr::value(serde_json::to_string(&grant).map_err(json_error)?),
        )
        .col_expr(
            agent_capability_grant::Column::Version,
            Expr::value(grant_row.version + 1),
        )
        .col_expr(agent_capability_grant::Column::UpdatedAt, Expr::value(now))
        .filter(agent_capability_grant::Column::Id.eq(grant_row.id))
        .filter(agent_capability_grant::Column::Version.eq(grant_row.version))
        .exec(db)
        .await?;
    if released.rows_affected != 1 || superseded.rows_affected != 1 || restored.rows_affected != 1 {
        return Err(DbErr::Custom(
            "capability pre-intent release conflicted".into(),
        ));
    }
    Ok(())
}

fn decode_prepared_payload(
    work: &agent_action_item::Model,
) -> Result<PreparedCapabilityPayload, DbErr> {
    serde_json::from_str(&work.payload_json).map_err(json_error)
}

fn validate_outbox_replay(
    outbox: &agent_capability_dispatch_outbox::Model,
    reservation: &agent_grant_reservation::Model,
    work: &agent_action_item::Model,
    request: &PrepareCapabilityCall<'_>,
) -> Result<(), DbErr> {
    let payload: CapabilityDispatchPayload =
        serde_json::from_str(&outbox.payload_json).map_err(json_error)?;
    let expected_dispatch_id = stable_id(
        "capability-dispatch",
        &format!("{}:{}", request.call_id, request.generation),
    );
    if outbox.dispatch_id != expected_dispatch_id
        || outbox.work_id != work.id
        || outbox.reservation_id != reservation.reservation_id
        || payload.dispatch_id != expected_dispatch_id
        || payload.canonical_input_json != request.canonical_input_json
        || payload.canonical_input_digest_sha256 != request.call.canonical_input_digest_sha256
    {
        return Err(DbErr::Custom(
            "stable dispatch id was replayed with different authority or input".into(),
        ));
    }
    Ok(())
}

async fn load_prepared<C: sea_orm::ConnectionTrait>(
    db: &C,
    call_id: &str,
) -> Result<Option<(agent_grant_reservation::Model, agent_action_item::Model)>, DbErr> {
    let Some(reservation) = agent_grant_reservation::Entity::find()
        .filter(agent_grant_reservation::Column::CallId.eq(call_id))
        .one(db)
        .await?
    else {
        return Ok(None);
    };
    let work = agent_action_item::Entity::find_by_id(reservation.work_id)
        .one(db)
        .await?
        .ok_or_else(|| DbErr::Custom("grant reservation has no prepared work".into()))?;
    Ok(Some((reservation, work)))
}

fn validate_replay(
    existing: &(agent_grant_reservation::Model, agent_action_item::Model),
    request: &PrepareCapabilityCall<'_>,
) -> Result<PreparedCapabilityCall, DbErr> {
    let (reservation, work) = existing;
    let payload: PreparedCapabilityPayload =
        serde_json::from_str(&work.payload_json).map_err(json_error)?;
    if reservation.grant_id != request.grant_id
        || reservation.run_id != request.call.run_id
        || reservation.canonical_input_digest_sha256 != request.call.canonical_input_digest_sha256
        || reservation.generation
            != i64::try_from(request.generation)
                .map_err(|_| DbErr::Custom("generation exceeds SQLite range".into()))?
        || payload.input_revision != request.input_revision
        || payload.input_watermark != request.input_watermark
        || payload.canonical_input_json != request.canonical_input_json
        || payload.provider_id != request.call.provider_id
        || payload.capability_id != request.call.capability_id
        || payload.tool_name != request.call.tool_name
    {
        return Err(DbErr::Custom(
            "stable call id was replayed with different authority or input".into(),
        ));
    }
    Ok(PreparedCapabilityCall {
        work_id: work.id,
        reservation_id: reservation.reservation_id.clone(),
        call_id: reservation.call_id.clone(),
        generation: request.generation,
        idempotent_replay: true,
    })
}

fn validate_prepare(request: &PrepareCapabilityCall<'_>) -> Result<(), DbErr> {
    for (field, value) in [
        ("grant_id", request.grant_id),
        ("call_id", request.call_id),
        ("turn_id", request.turn_id),
    ] {
        if value.trim().is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
            return Err(DbErr::Custom(format!("invalid {field}")));
        }
    }
    if request.input_revision == 0
        || request.input_watermark == 0
        || request.generation == 0
        || request.call.run_id.is_empty()
    {
        return Err(DbErr::Custom(
            "invalid prepare revision or generation".into(),
        ));
    }
    let canonical_digest = format!(
        "{:x}",
        Sha256::digest(request.canonical_input_json.as_bytes())
    );
    if canonical_digest != request.call.canonical_input_digest_sha256 {
        return Err(DbErr::Custom(
            "canonical input bytes do not match their digest".into(),
        ));
    }
    Ok(())
}

fn decode_grant(row: &agent_capability_grant::Model) -> Result<CapabilityGrant, DbErr> {
    let grant: CapabilityGrant = serde_json::from_str(&row.payload_json).map_err(json_error)?;
    grant
        .validate()
        .map_err(|error| DbErr::Custom(format!("invalid stored capability grant: {error}")))?;
    if i32::try_from(grant.remaining_uses).ok() != Some(row.remaining_uses) {
        return Err(DbErr::Custom(
            "capability grant use projection disagrees with payload".into(),
        ));
    }
    Ok(grant)
}

fn stable_id(prefix: &str, value: &str) -> String {
    format!("{prefix}-{:x}", Sha256::digest(value.as_bytes()))
}

fn timestamp(unix_ms: u64) -> Result<chrono::DateTime<Utc>, DbErr> {
    let unix_ms = i64::try_from(unix_ms)
        .map_err(|_| DbErr::Custom("timestamp exceeds SQLite range".into()))?;
    Utc.timestamp_millis_opt(unix_ms)
        .single()
        .ok_or_else(|| DbErr::Custom("invalid timestamp".into()))
}

fn json_error(error: serde_json::Error) -> DbErr {
    DbErr::Custom(format!("capability grant JSON: {error}"))
}

fn validate_completion(completion: &CapabilityDispatchCompletion) -> Result<(), DbErr> {
    if completion.dispatch_id.trim().is_empty()
        || completion.call_id.trim().is_empty()
        || completion.generation == 0
        || completion.result_digest_sha256.len() != 64
        || !completion
            .result_digest_sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(DbErr::Custom(
            "invalid capability dispatch completion".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    mod computer_binding;

    use std::path::Path;

    use super::*;
    use desk_agent_protocol::{
        AgentScope, ExecutionMode,
        capability_grant::{
            CapabilityGrantIssuer, CapabilityGrantLimits, CapabilityGrantUsePolicy,
            CapabilityRiskTier,
        },
        capability_provider::{CapabilityEffect, ProductSurface},
    };
    use sea_orm::{ConnectionTrait, Database, PaginatorTrait, Schema, Statement};

    const CRASH_DB_ENV: &str = "DESK_SIGNAL_CAPABILITY_CRASH_DB";
    const CRASH_MARKER_ENV: &str = "DESK_SIGNAL_CAPABILITY_CRASH_MARKER";
    const CRASH_PHASE_ENV: &str = "DESK_SIGNAL_CAPABILITY_CRASH_PHASE";

    fn grant(max_uses: u32) -> CapabilityGrant {
        CapabilityGrant {
            schema_version: CAPABILITY_GRANT_SCHEMA_VERSION,
            grant_id: "grant-1".into(),
            actor_id: "actor-1".into(),
            run_id: "run-1".into(),
            surface: ProductSurface::OssPersonalOwner,
            target_device_id: "device-1".into(),
            target_session_id: Some("session-1".into()),
            provider_id: "file.workspace".into(),
            capability_id: "file.artifact.create".into(),
            tool_name: "create_artifact".into(),
            tool_schema_version: 1,
            effect: CapabilityEffect::WriteArtifact,
            risk_tier: CapabilityRiskTier::R2,
            resource_scope: vec!["root:selected".into()],
            operation_scope: vec!["create_new".into()],
            export_destinations: Vec::new(),
            allowed_envelope_ids: Vec::new(),
            allowed_content_digests_sha256: Vec::new(),
            use_policy: CapabilityGrantUsePolicy::Reusable,
            canonical_input_digest_sha256: None,
            issued_by: CapabilityGrantIssuer::UserDecision,
            issued_at_unix_ms: 100,
            expires_at_unix_ms: 10_000,
            remaining_uses: max_uses,
            limits: CapabilityGrantLimits {
                max_bytes_per_call: 1024,
                max_items_per_call: 1,
                max_calls: max_uses,
            },
            policy_revision: 7,
            readiness_revision: 9,
            revoked_at_unix_ms: None,
            revoked_reason: None,
        }
    }

    async fn file_db(path: &std::path::Path) -> DatabaseConnection {
        let db = Database::connect(format!("sqlite://{}?mode=rwc", path.display()))
            .await
            .unwrap();
        db.execute_unprepared(
            "PRAGMA journal_mode = WAL; PRAGMA synchronous = FULL; PRAGMA foreign_keys = ON;",
        )
        .await
        .unwrap();
        let schema = Schema::new(db.get_database_backend());
        for statement in [
            schema.create_table_from_entity(agent_capability_grant::Entity),
            schema.create_table_from_entity(agent_grant_reservation::Entity),
            schema.create_table_from_entity(agent_capability_dispatch_outbox::Entity),
            schema.create_table_from_entity(agent_action_item::Entity),
            schema.create_table_from_entity(agent_session::Entity),
        ] {
            db.execute(&statement).await.unwrap();
        }
        db
    }

    async fn insert_session(db: &DatabaseConnection, input_revision: u64, input_seq: u64) {
        let mut session = PersistedAgentSession::new(
            "run-1",
            "actor-1",
            "device-1",
            7,
            AgentScope {
                granted: Vec::new(),
                mode: ExecutionMode::SuggestOnly,
                expires_at: None,
                policy_name: None,
            },
            "1970-01-01T00:00:00.100Z",
        );
        session.input_revision = input_revision;
        session.latest_input_seq = input_seq;
        agent_session::ActiveModel {
            conversation_id: Set("run-1".into()),
            actor_id: Set("actor-1".into()),
            device_id: Set("device-1".into()),
            state_json: Set(session.encode_json_for_storage().unwrap()),
            version: Set(1),
            lease_token: Set(0),
            lease_deadline: Set(None),
            created_at: Set(timestamp(100).unwrap()),
            updated_at: Set(timestamp(100).unwrap()),
            ..Default::default()
        }
        .insert(db)
        .await
        .unwrap();
    }

    async fn advance_session_input(db: &DatabaseConnection, input_revision: u64, input_seq: u64) {
        let row = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq("run-1"))
            .one(db)
            .await
            .unwrap()
            .unwrap();
        let mut session = PersistedAgentSession::decode_json(&row.state_json).unwrap();
        session.input_revision = input_revision;
        session.latest_input_seq = input_seq;
        let mut active: agent_session::ActiveModel = row.into();
        active.state_json = Set(session.encode_json_for_storage().unwrap());
        active.version = Set(2);
        active.update(db).await.unwrap();
    }

    #[allow(clippy::too_many_arguments)]
    fn request<'a>(
        call_id: &'a str,
        canonical_json: &'a str,
        canonical_digest: &'a str,
        resources: &'a [String],
        operations: &'a [String],
        generation: u64,
    ) -> PrepareCapabilityCall<'a> {
        PrepareCapabilityCall {
            grant_id: "grant-1",
            call_id,
            turn_id: "turn-1",
            input_revision: 1,
            input_watermark: 1,
            generation,
            canonical_input_json: canonical_json,
            call: CapabilityGrantCall {
                actor_id: "actor-1",
                run_id: "run-1",
                surface: ProductSurface::OssPersonalOwner,
                target_device_id: "device-1",
                target_session_id: Some("session-1"),
                provider_id: "file.workspace",
                capability_id: "file.artifact.create",
                tool_name: "create_artifact",
                tool_schema_version: 1,
                effect: CapabilityEffect::WriteArtifact,
                risk_tier: CapabilityRiskTier::R2,
                resource_scope: resources,
                operation_scope: operations,
                export_destinations: &[],
                envelope_ids: &[],
                content_digests_sha256: &[],
                canonical_input_digest_sha256: canonical_digest,
                byte_count: 10,
                item_count: 1,
                policy_revision: 7,
                readiness_revision: 9,
                now_unix_ms: 500,
            },
        }
    }

    #[test]
    fn capability_crash_child() {
        let Ok(path) = std::env::var(CRASH_DB_ENV) else {
            return;
        };
        let marker = std::env::var(CRASH_MARKER_ENV).unwrap();
        let phase = std::env::var(CRASH_PHASE_ENV).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let db = file_db(Path::new(&path)).await;
            insert_session(&db, 1, 1).await;
            let store = SignalCapabilityGrantStore::new(db);
            store.issue(&grant(1)).await.unwrap();
            let resources = vec!["root:selected".into()];
            let operations = vec!["create_new".into()];
            let canonical_json = r#"{"path":"crash.txt"}"#;
            let canonical = format!("{:x}", Sha256::digest(canonical_json.as_bytes()));
            store
                .prepare(request(
                    "call-crash",
                    canonical_json,
                    &canonical,
                    &resources,
                    &operations,
                    1,
                ))
                .await
                .unwrap();
            if matches!(
                phase.as_str(),
                "intent_before_commit" | "after_intent" | "after_claim"
            ) {
                store
                    .record_dispatch_intent(request(
                        "call-crash",
                        canonical_json,
                        &canonical,
                        &resources,
                        &operations,
                        1,
                    ))
                    .await
                    .unwrap();
            }
            if phase == "after_claim" {
                let dispatch_id = stable_id("capability-dispatch", "call-crash:1");
                let claimed = store.claim_dispatch(&dispatch_id, 600).await.unwrap();
                assert!(matches!(claimed, DispatchClaimResult::Claimed(_)));
            }
            assert!(matches!(
                phase.as_str(),
                "after_prepare" | "after_intent" | "after_claim"
            ));
            std::fs::write(marker, b"committed").unwrap();
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            }
        });
    }

    async fn kill_child_at_boundary(path: &Path, marker: &Path, phase: &str) {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("capability_grant_store::tests::capability_crash_child")
            .arg("--nocapture")
            .env(CRASH_DB_ENV, path)
            .env(CRASH_MARKER_ENV, marker)
            .env(CRASH_PHASE_ENV, phase)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let mut committed = false;
        for _ in 0..200 {
            if marker.exists() {
                committed = true;
                break;
            }
            if let Some(status) = child.try_wait().unwrap() {
                panic!("crash fixture exited before commit marker: {status}");
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(
            committed,
            "crash fixture did not reach the requested boundary"
        );
        child.kill().unwrap();
        let status = child.wait().unwrap();
        assert!(
            !status.success(),
            "fixture must be terminated by the parent"
        );
    }

    #[tokio::test]
    async fn prepared_reservation_and_work_are_atomic_and_replay_after_wal_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("grant-prepare.db");
        let db = file_db(&path).await;
        let store = SignalCapabilityGrantStore::new(db.clone());
        store.issue(&grant(1)).await.unwrap();
        let resources = vec!["root:selected".into()];
        let operations = vec!["create_new".into()];
        let canonical_json = r#"{"path":"report-a.txt"}"#;
        let canonical = format!("{:x}", Sha256::digest(canonical_json.as_bytes()));
        let prepared = store
            .prepare(request(
                "call-1",
                canonical_json,
                &canonical,
                &resources,
                &operations,
                1,
            ))
            .await
            .unwrap();
        assert!(!prepared.idempotent_replay);
        drop(store);
        db.close().await.unwrap();

        let reopened = Database::connect(format!("sqlite://{}?mode=rw", path.display()))
            .await
            .unwrap();
        let store = SignalCapabilityGrantStore::new(reopened.clone());
        let replay = store
            .prepare(request(
                "call-1",
                canonical_json,
                &canonical,
                &resources,
                &operations,
                1,
            ))
            .await
            .unwrap();
        assert!(replay.idempotent_replay);
        assert_eq!(replay.work_id, prepared.work_id);
        assert_eq!(replay.reservation_id, prepared.reservation_id);
        assert!(
            store
                .prepare(request(
                    "call-1",
                    r#"{"path":"report-b.txt"}"#,
                    &format!(
                        "{:x}",
                        Sha256::digest(r#"{"path":"report-b.txt"}"#.as_bytes())
                    ),
                    &resources,
                    &operations,
                    1,
                ))
                .await
                .is_err(),
            "same call id with different canonical input is rejected"
        );
        assert!(
            store
                .prepare(request(
                    "call-2",
                    r#"{"path":"report-b.txt"}"#,
                    &format!(
                        "{:x}",
                        Sha256::digest(r#"{"path":"report-b.txt"}"#.as_bytes())
                    ),
                    &resources,
                    &operations,
                    1,
                ))
                .await
                .is_err(),
            "the one reserved use cannot be consumed again"
        );
        let grant_row = agent_capability_grant::Entity::find()
            .filter(agent_capability_grant::Column::GrantId.eq("grant-1"))
            .one(&reopened)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(grant_row.remaining_uses, 0);
        assert_eq!(
            agent_grant_reservation::Entity::find()
                .count(&reopened)
                .await
                .unwrap(),
            1
        );
        let work = agent_action_item::Entity::find_by_id(prepared.work_id)
            .one(&reopened)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(work.status, CAPABILITY_WORK_PREPARED);
        assert!(work.dispatch_intent_at.is_none());
        reopened.close().await.unwrap();
    }

    #[tokio::test]
    async fn concurrent_one_shot_prepare_has_one_winner_and_one_complete_fact_set() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("grant-race.db");
        let db = file_db(&path).await;
        let store = SignalCapabilityGrantStore::new(db.clone());
        store.issue(&grant(1)).await.unwrap();
        let resources = vec!["root:selected".into()];
        let operations = vec!["create_new".into()];
        let canonical_json_a = r#"{"path":"a.txt"}"#;
        let canonical_json_b = r#"{"path":"b.txt"}"#;
        let canonical_a = format!("{:x}", Sha256::digest(canonical_json_a.as_bytes()));
        let canonical_b = format!("{:x}", Sha256::digest(canonical_json_b.as_bytes()));
        let (left, right) = tokio::join!(
            store.prepare(request(
                "call-race-a",
                canonical_json_a,
                &canonical_a,
                &resources,
                &operations,
                1,
            )),
            store.prepare(request(
                "call-race-b",
                canonical_json_b,
                &canonical_b,
                &resources,
                &operations,
                1,
            )),
        );
        assert_eq!(usize::from(left.is_ok()) + usize::from(right.is_ok()), 1);
        assert_eq!(
            agent_grant_reservation::Entity::find()
                .count(&db)
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            agent_action_item::Entity::find()
                .filter(agent_action_item::Column::Kind.eq(CAPABILITY_WORK_KIND))
                .count(&db)
                .await
                .unwrap(),
            1
        );
        db.close().await.unwrap();
    }

    #[tokio::test]
    async fn reusable_grant_allows_only_bounded_in_scope_calls_without_reprompt() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("grant-reusable.db");
        let db = file_db(&path).await;
        let store = SignalCapabilityGrantStore::new(db.clone());
        store.issue(&grant(2)).await.unwrap();
        let resources = vec!["root:selected".into()];
        let outside = vec!["root:outside".into()];
        let create = vec!["create_new".into()];
        let overwrite = vec!["overwrite".into()];
        let first_json = r#"{"path":"first.txt"}"#;
        let first_digest = format!("{:x}", Sha256::digest(first_json.as_bytes()));

        assert!(
            store
                .prepare(request(
                    "call-outside",
                    first_json,
                    &first_digest,
                    &outside,
                    &create,
                    1,
                ))
                .await
                .is_err()
        );
        assert!(
            store
                .prepare(request(
                    "call-overwrite",
                    first_json,
                    &first_digest,
                    &resources,
                    &overwrite,
                    1,
                ))
                .await
                .is_err()
        );
        let mut oversized = request(
            "call-oversized",
            first_json,
            &first_digest,
            &resources,
            &create,
            1,
        );
        oversized.call.byte_count = 1_025;
        assert!(store.prepare(oversized).await.is_err());
        let untouched = agent_capability_grant::Entity::find()
            .filter(agent_capability_grant::Column::GrantId.eq("grant-1"))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(untouched.remaining_uses, 2);

        store
            .prepare(request(
                "call-first",
                first_json,
                &first_digest,
                &resources,
                &create,
                1,
            ))
            .await
            .unwrap();
        let second_json = r#"{"path":"second.txt"}"#;
        let second_digest = format!("{:x}", Sha256::digest(second_json.as_bytes()));
        store
            .prepare(request(
                "call-second",
                second_json,
                &second_digest,
                &resources,
                &create,
                1,
            ))
            .await
            .unwrap();
        assert!(
            store
                .prepare(request(
                    "call-third",
                    r#"{"path":"third.txt"}"#,
                    &format!("{:x}", Sha256::digest(r#"{"path":"third.txt"}"#.as_bytes())),
                    &resources,
                    &create,
                    1,
                ))
                .await
                .is_err(),
            "the reusable grant must stop at its durable max_calls limit"
        );
        let exhausted = agent_capability_grant::Entity::find()
            .filter(agent_capability_grant::Column::GrantId.eq("grant-1"))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(exhausted.remaining_uses, 0);
        assert_eq!(
            agent_grant_reservation::Entity::find()
                .count(&db)
                .await
                .unwrap(),
            2
        );
        db.close().await.unwrap();
    }

    #[tokio::test]
    async fn dispatch_intent_commits_reservation_work_and_exact_outbox_atomically() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("grant-intent.db");
        let db = file_db(&path).await;
        insert_session(&db, 1, 1).await;
        let store = SignalCapabilityGrantStore::new(db.clone());
        store.issue(&grant(1)).await.unwrap();
        let resources = vec!["root:selected".into()];
        let operations = vec!["create_new".into()];
        let canonical_json = r#"{"path":"report.txt"}"#;
        let canonical = format!("{:x}", Sha256::digest(canonical_json.as_bytes()));
        store
            .prepare(request(
                "call-intent",
                canonical_json,
                &canonical,
                &resources,
                &operations,
                1,
            ))
            .await
            .unwrap();
        let first = store
            .record_dispatch_intent(request(
                "call-intent",
                canonical_json,
                &canonical,
                &resources,
                &operations,
                1,
            ))
            .await
            .unwrap();
        let DispatchIntentResult::Recorded {
            dispatch_id,
            outbox_id,
            idempotent_replay,
        } = first
        else {
            panic!("current input must record an intent")
        };
        assert!(!idempotent_replay);
        let reservation = agent_grant_reservation::Entity::find()
            .filter(agent_grant_reservation::Column::CallId.eq("call-intent"))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let work = agent_action_item::Entity::find_by_id(reservation.work_id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let outbox = agent_capability_dispatch_outbox::Entity::find_by_id(outbox_id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reservation.state, RESERVATION_STATUS_COMMITTED);
        assert_eq!(work.status, CAPABILITY_WORK_INTENT_RECORDED);
        assert!(work.dispatch_intent_at.is_some());
        assert_eq!(outbox.state, DISPATCH_OUTBOX_PENDING);
        let payload: CapabilityDispatchPayload =
            serde_json::from_str(&outbox.payload_json).unwrap();
        assert_eq!(payload.dispatch_id, dispatch_id);
        assert_eq!(payload.canonical_input_json, canonical_json);
        assert_eq!(payload.canonical_input_digest_sha256, canonical);
        drop(store);
        db.close().await.unwrap();

        let reopened = Database::connect(format!("sqlite://{}?mode=rw", path.display()))
            .await
            .unwrap();
        let replay = SignalCapabilityGrantStore::new(reopened.clone())
            .record_dispatch_intent(request(
                "call-intent",
                canonical_json,
                &canonical,
                &resources,
                &operations,
                1,
            ))
            .await
            .unwrap();
        assert_eq!(
            replay,
            DispatchIntentResult::Recorded {
                dispatch_id,
                outbox_id,
                idempotent_replay: true,
            }
        );
        let grant = agent_capability_grant::Entity::find()
            .filter(agent_capability_grant::Column::GrantId.eq("grant-1"))
            .one(&reopened)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(grant.remaining_uses, 0, "intent never restores the use");
        reopened.close().await.unwrap();
    }

    #[tokio::test]
    async fn late_completion_refines_unknown_once_without_refunding_or_retrying() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("grant-late-completion.db");
        let db = file_db(&path).await;
        insert_session(&db, 1, 1).await;
        let store = SignalCapabilityGrantStore::new(db.clone());
        store.issue(&grant(1)).await.unwrap();
        let resources = vec!["root:selected".into()];
        let operations = vec!["create_new".into()];
        let canonical_json = r#"{"path":"late.txt"}"#;
        let canonical = format!("{:x}", Sha256::digest(canonical_json.as_bytes()));
        store
            .prepare(request(
                "call-late",
                canonical_json,
                &canonical,
                &resources,
                &operations,
                1,
            ))
            .await
            .unwrap();
        let intent = store
            .record_dispatch_intent(request(
                "call-late",
                canonical_json,
                &canonical,
                &resources,
                &operations,
                1,
            ))
            .await
            .unwrap();
        let DispatchIntentResult::Recorded { dispatch_id, .. } = intent else {
            panic!("current input must record an intent")
        };
        let claimed = store.claim_dispatch(&dispatch_id, 600).await.unwrap();
        assert!(matches!(claimed, DispatchClaimResult::Claimed(_)));
        let unknown = store
            .mark_dispatch_outcome_unknown(&dispatch_id, "call-late", 1, 650)
            .await
            .unwrap();
        assert!(!unknown.idempotent_replay);
        assert_eq!(
            store
                .mark_dispatch_outcome_unknown(&dispatch_id, "call-late", 1, 651)
                .await
                .unwrap(),
            DispatchUnknownResult {
                work_id: unknown.work_id,
                idempotent_replay: true,
            }
        );
        assert_eq!(
            store
                .manually_dispose_unknown_for_subject(
                    unknown.work_id,
                    "wrong-dispatch",
                    "run-1",
                    "actor-1",
                    "device-1",
                    675,
                )
                .await
                .unwrap(),
            CapabilityManualDispositionResult::SubjectMismatch
        );
        assert_eq!(
            store
                .manually_dispose_unknown_for_subject(
                    unknown.work_id,
                    &dispatch_id,
                    "run-1",
                    "actor-1",
                    "device-1",
                    675,
                )
                .await
                .unwrap(),
            CapabilityManualDispositionResult::Applied
        );
        assert_eq!(
            store
                .manually_dispose_unknown_for_subject(
                    unknown.work_id,
                    &dispatch_id,
                    "run-1",
                    "actor-1",
                    "device-1",
                    676,
                )
                .await
                .unwrap(),
            CapabilityManualDispositionResult::AlreadyResolved
        );
        let completion = CapabilityDispatchCompletion {
            dispatch_id: dispatch_id.clone(),
            call_id: "call-late".into(),
            generation: 1,
            outcome: CapabilityDispatchOutcome::Succeeded,
            result_digest_sha256: format!("{:x}", Sha256::digest(b"verified result")),
        };
        let first = store
            .record_dispatch_completion(&completion, 700)
            .await
            .unwrap();
        assert!(!first.idempotent_replay);
        assert_eq!(
            store
                .record_dispatch_completion(&completion, 701)
                .await
                .unwrap(),
            DispatchCompletionResult {
                work_id: first.work_id,
                idempotent_replay: true,
            }
        );
        let conflicting = CapabilityDispatchCompletion {
            outcome: CapabilityDispatchOutcome::Failed,
            ..completion.clone()
        };
        assert!(
            store
                .record_dispatch_completion(&conflicting, 702)
                .await
                .is_err()
        );
        let outbox = agent_capability_dispatch_outbox::Entity::find()
            .filter(agent_capability_dispatch_outbox::Column::DispatchId.eq(dispatch_id))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let work = agent_action_item::Entity::find_by_id(first.work_id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let stored_grant = agent_capability_grant::Entity::find()
            .filter(agent_capability_grant::Column::GrantId.eq("grant-1"))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(outbox.state, DISPATCH_OUTBOX_COMPLETED);
        assert_eq!(work.status, CAPABILITY_WORK_SUCCEEDED);
        assert!(work.manual_resolved_at.is_some());
        assert_eq!(stored_grant.remaining_uses, 0);
        db.close().await.unwrap();
    }

    #[tokio::test]
    async fn completion_retries_sqlite_writer_contention_without_replaying_dispatch() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("grant-completion-contention.db");
        let db = file_db(&path).await;
        insert_session(&db, 1, 1).await;
        let store = SignalCapabilityGrantStore::new(db.clone());
        store.issue(&grant(1)).await.unwrap();
        let resources = vec!["root:selected".into()];
        let operations = vec!["create_new".into()];
        let canonical_json = r#"{"path":"contended.txt"}"#;
        let canonical = format!("{:x}", Sha256::digest(canonical_json.as_bytes()));
        store
            .prepare(request(
                "call-contended",
                canonical_json,
                &canonical,
                &resources,
                &operations,
                1,
            ))
            .await
            .unwrap();
        let intent = store
            .record_dispatch_intent(request(
                "call-contended",
                canonical_json,
                &canonical,
                &resources,
                &operations,
                1,
            ))
            .await
            .unwrap();
        let DispatchIntentResult::Recorded { dispatch_id, .. } = intent else {
            panic!("current input must record an intent")
        };
        let claimed = store.claim_dispatch(&dispatch_id, 600).await.unwrap();
        let DispatchClaimResult::Claimed(payload) = claimed else {
            panic!("dispatch must be claimed exactly once")
        };

        let lock_path = path.clone();
        let lock_work_id = payload.work_id;
        let (lock_ready_tx, lock_ready_rx) = std::sync::mpsc::sync_channel(1);
        // Keep the competing writer on an independent OS thread and runtime.
        // This models another process and prevents the fixture's unlock from
        // being queued behind completion retries on SQLx's test worker pool.
        let lock_thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let lock_db =
                    Database::connect(format!("sqlite://{}?mode=rw", lock_path.display()))
                        .await
                        .unwrap();
                lock_db
                    .execute_unprepared("PRAGMA busy_timeout = 1000")
                    .await
                    .unwrap();
                let lock_txn = lock_db.begin().await.unwrap();
                agent_action_item::Entity::update_many()
                    .col_expr(
                        agent_action_item::Column::UpdatedAt,
                        Expr::value(timestamp(601).unwrap()),
                    )
                    .filter(agent_action_item::Column::Id.eq(lock_work_id))
                    .exec(&lock_txn)
                    .await
                    .unwrap();
                lock_ready_tx.send(()).unwrap();
                std::thread::sleep(std::time::Duration::from_millis(100));
                lock_txn.commit().await.unwrap();
                lock_db.close().await.unwrap();
            });
        });
        lock_ready_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();

        let completion = CapabilityDispatchCompletion {
            dispatch_id: dispatch_id.clone(),
            call_id: "call-contended".into(),
            generation: 1,
            outcome: CapabilityDispatchOutcome::Succeeded,
            result_digest_sha256: format!("{:x}", Sha256::digest(b"verified result")),
        };
        let completion_store = store.clone();
        let completion_task = tokio::spawn(async move {
            completion_store
                .record_dispatch_completion(&completion, 700)
                .await
        });
        let unlock_task = tokio::task::spawn_blocking(move || lock_thread.join().unwrap());

        let result = completion_task.await.unwrap().unwrap();
        unlock_task.await.unwrap();
        assert_eq!(result.work_id, payload.work_id);
        assert!(!result.idempotent_replay);
        let outbox = agent_capability_dispatch_outbox::Entity::find()
            .filter(agent_capability_dispatch_outbox::Column::DispatchId.eq(dispatch_id))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let work = agent_action_item::Entity::find_by_id(payload.work_id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let action_count = agent_action_item::Entity::find().count(&db).await.unwrap();
        assert_eq!(outbox.state, DISPATCH_OUTBOX_COMPLETED);
        assert_eq!(work.status, CAPABILITY_WORK_SUCCEEDED);
        assert_eq!(
            action_count, 1,
            "database retry must not replay the Provider call"
        );
        db.close().await.unwrap();
    }

    #[tokio::test]
    async fn child_kill_reopens_every_dispatch_boundary_conservatively() {
        for phase in [
            "prepare_before_commit",
            "after_prepare",
            "intent_before_commit",
            "after_intent",
            "after_claim",
        ] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join(format!("grant-{phase}.db"));
            let marker = directory.path().join("boundary.marker");
            kill_child_at_boundary(&path, &marker, phase).await;

            let reopened = Database::connect(format!("sqlite://{}?mode=rw", path.display()))
                .await
                .unwrap();
            let quick_check: String = reopened
                .query_one_raw(Statement::from_string(
                    reopened.get_database_backend(),
                    "PRAGMA quick_check".to_string(),
                ))
                .await
                .unwrap()
                .unwrap()
                .try_get("", "quick_check")
                .unwrap();
            assert_eq!(quick_check, "ok");
            let grant_row = agent_capability_grant::Entity::find()
                .filter(agent_capability_grant::Column::GrantId.eq("grant-1"))
                .one(&reopened)
                .await
                .unwrap()
                .unwrap();
            let reservation = agent_grant_reservation::Entity::find()
                .filter(agent_grant_reservation::Column::CallId.eq("call-crash"))
                .one(&reopened)
                .await
                .unwrap();
            if phase == "prepare_before_commit" {
                assert_eq!(grant_row.remaining_uses, 1);
                assert!(reservation.is_none());
                assert_eq!(
                    agent_action_item::Entity::find()
                        .filter(agent_action_item::Column::Kind.eq(CAPABILITY_WORK_KIND))
                        .count(&reopened)
                        .await
                        .unwrap(),
                    0
                );
                assert_eq!(
                    agent_capability_dispatch_outbox::Entity::find()
                        .count(&reopened)
                        .await
                        .unwrap(),
                    0
                );
                reopened.close().await.unwrap();
                continue;
            }
            let reservation = reservation.unwrap();
            let work = agent_action_item::Entity::find_by_id(reservation.work_id)
                .one(&reopened)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(grant_row.remaining_uses, 0);
            assert_eq!(reservation.generation, 1);

            if matches!(phase, "after_prepare" | "intent_before_commit") {
                assert_eq!(reservation.state, RESERVATION_STATUS_RESERVED);
                assert_eq!(work.status, CAPABILITY_WORK_PREPARED);
                assert_eq!(
                    agent_capability_dispatch_outbox::Entity::find()
                        .count(&reopened)
                        .await
                        .unwrap(),
                    0
                );
                let resources = vec!["root:selected".into()];
                let operations = vec!["create_new".into()];
                let canonical_json = r#"{"path":"crash.txt"}"#;
                let canonical = format!("{:x}", Sha256::digest(canonical_json.as_bytes()));
                let replay = SignalCapabilityGrantStore::new(reopened.clone())
                    .prepare(request(
                        "call-crash",
                        canonical_json,
                        &canonical,
                        &resources,
                        &operations,
                        1,
                    ))
                    .await
                    .unwrap();
                assert!(replay.idempotent_replay);
            } else {
                assert_eq!(reservation.state, RESERVATION_STATUS_COMMITTED);
                let expected_outbox = if phase == "after_claim" {
                    DISPATCH_OUTBOX_SENDING
                } else {
                    DISPATCH_OUTBOX_PENDING
                };
                let expected_work = if phase == "after_claim" {
                    CAPABILITY_WORK_DISPATCHING
                } else {
                    CAPABILITY_WORK_INTENT_RECORDED
                };
                assert_eq!(work.status, expected_work);
                let before_recovery = agent_capability_dispatch_outbox::Entity::find()
                    .filter(agent_capability_dispatch_outbox::Column::CallId.eq("call-crash"))
                    .one(&reopened)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(before_recovery.state, expected_outbox);
                assert_eq!(
                    SignalCapabilityGrantStore::new(reopened.clone())
                        .recover_unfinished_dispatches_after_restart(800)
                        .await
                        .unwrap(),
                    1
                );
                let outbox = agent_capability_dispatch_outbox::Entity::find()
                    .filter(agent_capability_dispatch_outbox::Column::CallId.eq("call-crash"))
                    .one(&reopened)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(outbox.state, DISPATCH_OUTBOX_OUTCOME_UNKNOWN);
                let recovered = agent_action_item::Entity::find_by_id(work.id)
                    .one(&reopened)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(recovered.status, CAPABILITY_WORK_OUTCOME_UNKNOWN);
            }
            reopened.close().await.unwrap();
        }
    }

    #[tokio::test]
    async fn newer_user_input_before_intent_releases_exactly_once_and_writes_no_outbox() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("grant-superseded.db");
        let db = file_db(&path).await;
        insert_session(&db, 1, 1).await;
        let store = SignalCapabilityGrantStore::new(db.clone());
        store.issue(&grant(1)).await.unwrap();
        let resources = vec!["root:selected".into()];
        let operations = vec!["create_new".into()];
        let canonical_json = r#"{"path":"stale.txt"}"#;
        let canonical = format!("{:x}", Sha256::digest(canonical_json.as_bytes()));
        let prepared = store
            .prepare(request(
                "call-stale",
                canonical_json,
                &canonical,
                &resources,
                &operations,
                1,
            ))
            .await
            .unwrap();
        advance_session_input(&db, 2, 2).await;
        assert_eq!(
            store
                .record_dispatch_intent(request(
                    "call-stale",
                    canonical_json,
                    &canonical,
                    &resources,
                    &operations,
                    1,
                ))
                .await
                .unwrap(),
            DispatchIntentResult::SupersededBeforeIntent {
                work_id: prepared.work_id,
                idempotent_replay: false,
            }
        );
        assert_eq!(
            store
                .record_dispatch_intent(request(
                    "call-stale",
                    canonical_json,
                    &canonical,
                    &resources,
                    &operations,
                    1,
                ))
                .await
                .unwrap(),
            DispatchIntentResult::SupersededBeforeIntent {
                work_id: prepared.work_id,
                idempotent_replay: true,
            }
        );
        let grant = agent_capability_grant::Entity::find()
            .filter(agent_capability_grant::Column::GrantId.eq("grant-1"))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let reservation = agent_grant_reservation::Entity::find()
            .filter(agent_grant_reservation::Column::CallId.eq("call-stale"))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let work = agent_action_item::Entity::find_by_id(prepared.work_id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(grant.remaining_uses, 1);
        assert_eq!(reservation.state, RESERVATION_STATUS_RELEASED);
        assert_eq!(work.status, CAPABILITY_WORK_SUPERSEDED);
        assert!(work.dispatch_intent_at.is_none());
        assert_eq!(
            agent_capability_dispatch_outbox::Entity::find()
                .count(&db)
                .await
                .unwrap(),
            0
        );
        db.close().await.unwrap();
    }

    #[tokio::test]
    async fn restart_after_intent_marks_pending_and_sending_unknown_without_retry_or_refund() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("grant-restart.db");
        let db = file_db(&path).await;
        insert_session(&db, 1, 1).await;
        let store = SignalCapabilityGrantStore::new(db.clone());
        store.issue(&grant(2)).await.unwrap();
        let resources = vec!["root:selected".into()];
        let operations = vec!["create_new".into()];
        let pending_json = r#"{"path":"pending.txt"}"#;
        let sending_json = r#"{"path":"sending.txt"}"#;
        let pending_digest = format!("{:x}", Sha256::digest(pending_json.as_bytes()));
        let sending_digest = format!("{:x}", Sha256::digest(sending_json.as_bytes()));
        for (call_id, canonical_json, digest) in [
            ("call-pending", pending_json, pending_digest.as_str()),
            ("call-sending", sending_json, sending_digest.as_str()),
        ] {
            store
                .prepare(request(
                    call_id,
                    canonical_json,
                    digest,
                    &resources,
                    &operations,
                    1,
                ))
                .await
                .unwrap();
            store
                .record_dispatch_intent(request(
                    call_id,
                    canonical_json,
                    digest,
                    &resources,
                    &operations,
                    1,
                ))
                .await
                .unwrap();
        }
        let sending_row = agent_capability_dispatch_outbox::Entity::find()
            .filter(agent_capability_dispatch_outbox::Column::CallId.eq("call-sending"))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        let claimed = store
            .claim_dispatch(&sending_row.dispatch_id, 600)
            .await
            .unwrap();
        let DispatchClaimResult::Claimed(payload) = claimed else {
            panic!("fresh intent must be claimable exactly once")
        };
        assert_eq!(payload.canonical_input_json, sending_json);
        assert_eq!(payload.canonical_input_digest_sha256, sending_digest);
        drop(store);
        db.close().await.unwrap();

        let reopened = Database::connect(format!("sqlite://{}?mode=rw", path.display()))
            .await
            .unwrap();
        let store = SignalCapabilityGrantStore::new(reopened.clone());
        assert_eq!(
            store
                .recover_unfinished_dispatches_after_restart(700)
                .await
                .unwrap(),
            2,
            "both pre-handoff and handoff-in-progress intents are uncertain after restart"
        );
        assert_eq!(
            store
                .recover_unfinished_dispatches_after_restart(701)
                .await
                .unwrap(),
            0,
            "recovery is idempotent"
        );
        let rows = agent_capability_dispatch_outbox::Entity::find()
            .all(&reopened)
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);
        for row in rows {
            assert_eq!(row.state, DISPATCH_OUTBOX_OUTCOME_UNKNOWN);
            assert_eq!(
                store.claim_dispatch(&row.dispatch_id, 800).await.unwrap(),
                DispatchClaimResult::OutcomeUnknown {
                    dispatch_id: row.dispatch_id.clone(),
                }
            );
            let work = agent_action_item::Entity::find_by_id(row.work_id)
                .one(&reopened)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(work.status, CAPABILITY_WORK_OUTCOME_UNKNOWN);
        }
        let grant = agent_capability_grant::Entity::find()
            .filter(agent_capability_grant::Column::GrantId.eq("grant-1"))
            .one(&reopened)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            grant.remaining_uses, 0,
            "uncertain intents are never refunded"
        );
        reopened.close().await.unwrap();
    }

    #[tokio::test]
    async fn revocation_blocks_new_reserve_and_releases_prepared_use_before_intent() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("grant-revoke.db");
        let db = file_db(&path).await;
        insert_session(&db, 1, 1).await;
        let store = SignalCapabilityGrantStore::new(db.clone());
        store.issue(&grant(1)).await.unwrap();
        let resources = vec!["root:selected".into()];
        let operations = vec!["create_new".into()];
        let canonical_json = r#"{"path":"revoked.txt"}"#;
        let canonical = format!("{:x}", Sha256::digest(canonical_json.as_bytes()));
        let prepared = store
            .prepare(request(
                "call-revoked",
                canonical_json,
                &canonical,
                &resources,
                &operations,
                1,
            ))
            .await
            .unwrap();
        let revoked = store
            .revoke("grant-1", "actor-1", "device-1", 600, "owner revoked")
            .await
            .unwrap();
        assert_eq!(revoked.revoked_at_unix_ms, Some(600));
        assert!(
            store
                .prepare(request(
                    "call-after-revoke",
                    canonical_json,
                    &canonical,
                    &resources,
                    &operations,
                    1,
                ))
                .await
                .is_err()
        );
        assert_eq!(
            store
                .record_dispatch_intent(request(
                    "call-revoked",
                    canonical_json,
                    &canonical,
                    &resources,
                    &operations,
                    1,
                ))
                .await
                .unwrap(),
            DispatchIntentResult::RevokedBeforeIntent {
                work_id: prepared.work_id,
                idempotent_replay: false,
            }
        );
        assert_eq!(
            store
                .record_dispatch_intent(request(
                    "call-revoked",
                    canonical_json,
                    &canonical,
                    &resources,
                    &operations,
                    1,
                ))
                .await
                .unwrap(),
            DispatchIntentResult::RevokedBeforeIntent {
                work_id: prepared.work_id,
                idempotent_replay: true,
            }
        );
        let grants = store
            .list_for_subject("run-1", "actor-1", "device-1")
            .await
            .unwrap();
        assert_eq!(grants.len(), 1);
        assert_eq!(grants[0].remaining_uses, 1);
        assert_eq!(grants[0].revoked_reason.as_deref(), Some("owner revoked"));
        assert_eq!(
            agent_capability_dispatch_outbox::Entity::find()
                .count(&db)
                .await
                .unwrap(),
            0
        );
        db.close().await.unwrap();
    }
}
