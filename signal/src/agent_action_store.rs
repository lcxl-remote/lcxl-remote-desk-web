//! SQLite durable-action lifecycle for the single-node OSS signal runtime.

use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use desk_diagnose_core::durable_action::{
    CancelRequestOutcome, ClaimedAction, DurableActionLifecycle, RecoveredAction, WritebackOutcome,
};
use desk_diagnose_core::session::WorkKind;
use sea_orm::sea_query::Expr;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, DbErr, EntityTrait, ExprTrait, QueryFilter,
    Set,
};
use serde_json::Value;
use uuid::Uuid;

use crate::entity::agent_action_item;

pub const STATUS_AWAITING_APPROVAL: &str = "awaiting_approval";
pub const STATUS_APPROVED: &str = "approved";
pub const STATUS_REJECTED: &str = "rejected";
pub const STATUS_EXPIRED: &str = "expired";
pub const STATUS_CLAIMED: &str = "claimed";
pub const STATUS_DISPATCHED: &str = "dispatched";
pub const STATUS_DONE: &str = "done";
pub const STATUS_UNKNOWN: &str = "unknown";
pub const STATUS_CANCELLED: &str = "cancelled";

#[derive(Debug, Clone)]
pub struct PersonalActionDraft {
    pub kind: WorkKind,
    pub action_request_id: String,
    pub conversation_id: String,
    pub turn_id: String,
    pub tool_call_id: String,
    pub owner_actor_id: String,
    pub target_device_id: String,
    pub policy_revision: i64,
    pub draft_hash: String,
    pub payload_json: String,
    pub payload_schema_version: u16,
    pub is_side_effecting: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalOutcome {
    Approved,
    Rejected,
    AlreadyResolved,
    SubjectMismatch,
    Expired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManualDispositionOutcome {
    Applied,
    AlreadyResolved,
    SubjectMismatch,
    StateMismatch,
}

#[derive(Clone)]
pub struct SignalActionStore {
    db: DatabaseConnection,
}

impl SignalActionStore {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    /// Create a personal-owner approval. The table intentionally has no org
    /// resolution column, so an organization member can never become an alternate
    /// approver in OSS.
    pub async fn create_pending(
        &self,
        draft: PersonalActionDraft,
        approval_ttl: Duration,
    ) -> Result<agent_action_item::Model, DbErr> {
        if draft.kind == WorkKind::AgentExec {
            return Err(DbErr::Custom(
                "generic Signal action store does not replace confirmed exec".into(),
            ));
        }
        let now = Utc::now();
        let ttl = chrono::Duration::from_std(approval_ttl)
            .unwrap_or_else(|_| chrono::Duration::seconds(120));
        let action_request_id = draft.action_request_id;
        agent_action_item::ActiveModel {
            kind: Set(draft.kind.as_str().into()),
            action_request_id: Set(action_request_id.clone()),
            exec_request_id: Set(None),
            conversation_id: Set(draft.conversation_id),
            turn_id: Set(draft.turn_id),
            tool_call_id: Set(draft.tool_call_id),
            actor_id: Set(draft.owner_actor_id),
            target_device_id: Set(draft.target_device_id),
            status: Set(STATUS_AWAITING_APPROVAL.into()),
            attempt: Set(0),
            approval_expires_at: Set(Some(now + ttl)),
            draft_hash: Set(draft.draft_hash),
            policy_revision: Set(draft.policy_revision),
            is_side_effecting: Set(draft.is_side_effecting),
            payload_json: Set(draft.payload_json),
            payload_schema_version: Set(i32::from(draft.payload_schema_version)),
            completion_event_id: Set(format!("signal-action:{action_request_id}:done")),
            completion_delivery_state: Set("pending".into()),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(&self.db)
        .await
    }

    /// Resolve approval against the exact personal owner subject captured at
    /// draft time. This check is repeated again by [`claim_personal_by_id`] at the
    /// dispatch boundary.
    pub async fn resolve_personal(
        &self,
        work_id: i64,
        actor_id: &str,
        target_device_id: &str,
        approve: bool,
    ) -> Result<ApprovalOutcome, DbErr> {
        let Some(row) = agent_action_item::Entity::find_by_id(work_id)
            .one(&self.db)
            .await?
        else {
            return Ok(ApprovalOutcome::SubjectMismatch);
        };
        if row.actor_id != actor_id || row.target_device_id != target_device_id {
            return Ok(ApprovalOutcome::SubjectMismatch);
        }
        if row.status != STATUS_AWAITING_APPROVAL {
            return Ok(ApprovalOutcome::AlreadyResolved);
        }
        let now = Utc::now();
        if row.approval_expires_at.is_some_and(|expiry| expiry <= now) {
            let result = agent_action_item::Entity::update_many()
                .col_expr(
                    agent_action_item::Column::Status,
                    Expr::value(STATUS_EXPIRED),
                )
                .col_expr(agent_action_item::Column::UpdatedAt, Expr::value(now))
                .filter(agent_action_item::Column::Id.eq(work_id))
                .filter(agent_action_item::Column::Status.eq(STATUS_AWAITING_APPROVAL))
                .exec(&self.db)
                .await?;
            return Ok(if result.rows_affected == 1 {
                ApprovalOutcome::Expired
            } else {
                ApprovalOutcome::AlreadyResolved
            });
        }
        let status = if approve {
            STATUS_APPROVED
        } else {
            STATUS_REJECTED
        };
        let approval_id = approve.then(|| Uuid::new_v4().to_string());
        let result = agent_action_item::Entity::update_many()
            .col_expr(agent_action_item::Column::Status, Expr::value(status))
            .col_expr(
                agent_action_item::Column::ApprovalId,
                Expr::value(approval_id),
            )
            .col_expr(
                agent_action_item::Column::ApprovedAt,
                Expr::value(approve.then_some(now)),
            )
            .col_expr(agent_action_item::Column::UpdatedAt, Expr::value(now))
            .filter(agent_action_item::Column::Id.eq(work_id))
            .filter(agent_action_item::Column::Status.eq(STATUS_AWAITING_APPROVAL))
            .exec(&self.db)
            .await?;
        Ok(if result.rows_affected == 1 {
            if approve {
                ApprovalOutcome::Approved
            } else {
                ApprovalOutcome::Rejected
            }
        } else {
            ApprovalOutcome::AlreadyResolved
        })
    }

    /// Record an owner's explicit disposition of the exact unresolved action.
    /// This is an audit fact only: the action remains `unknown`, no grant use is
    /// restored, and a late provider result may still be recorded separately.
    pub async fn manually_dispose_for_subject(
        &self,
        work_id: i64,
        execution_id: &str,
        conversation_id: &str,
        actor_id: &str,
        target_device_id: &str,
    ) -> Result<ManualDispositionOutcome, DbErr> {
        let Some(row) = agent_action_item::Entity::find_by_id(work_id)
            .one(&self.db)
            .await?
        else {
            return Ok(ManualDispositionOutcome::SubjectMismatch);
        };
        if row.conversation_id != conversation_id
            || row.actor_id != actor_id
            || row.target_device_id != target_device_id
            || row.execution_id.as_deref() != Some(execution_id)
        {
            return Ok(ManualDispositionOutcome::SubjectMismatch);
        }
        if row.manual_resolved_at.is_some() {
            return Ok(ManualDispositionOutcome::AlreadyResolved);
        }
        if !matches!(row.status.as_str(), STATUS_DISPATCHED | STATUS_UNKNOWN) {
            return Ok(ManualDispositionOutcome::StateMismatch);
        }
        Ok(match self.manual_resolve(work_id).await? {
            WritebackOutcome::Applied => ManualDispositionOutcome::Applied,
            WritebackOutcome::AlreadyResolved => ManualDispositionOutcome::AlreadyResolved,
            WritebackOutcome::Stale => ManualDispositionOutcome::StateMismatch,
        })
    }

    pub async fn claim_personal_by_id(
        &self,
        work_id: i64,
        actor_id: &str,
        target_device_id: &str,
        node_id: &str,
        lease: Duration,
    ) -> Result<Option<ClaimedAction>, DbErr> {
        let Some(row) = agent_action_item::Entity::find_by_id(work_id)
            .one(&self.db)
            .await?
        else {
            return Ok(None);
        };
        if row.actor_id != actor_id || row.target_device_id != target_device_id {
            return Ok(None);
        }
        self.claim_by_id(work_id, node_id, lease).await
    }

    async fn row_for_claim_token(
        &self,
        claim_token: &str,
    ) -> Result<Option<agent_action_item::Model>, DbErr> {
        agent_action_item::Entity::find()
            .filter(agent_action_item::Column::ClaimToken.eq(claim_token))
            .one(&self.db)
            .await
    }
}

fn claimed(row: agent_action_item::Model) -> Result<ClaimedAction, DbErr> {
    Ok(ClaimedAction {
        work_id: row.id,
        claim_token: row
            .claim_token
            .ok_or_else(|| DbErr::Custom("claimed action has no token".into()))?,
        attempt: row.attempt,
        kind: WorkKind::from_persisted(&row.kind)
            .ok_or_else(|| DbErr::Custom(format!("unknown action kind {}", row.kind)))?,
        action_request_id: row.action_request_id,
        exec_request_id: row.exec_request_id,
        approval_id: row.approval_id,
        payload_json: row.payload_json,
        payload_schema_version: u16::try_from(row.payload_schema_version)
            .map_err(|_| DbErr::Custom("invalid payload schema version".into()))?,
        is_side_effecting: row.is_side_effecting,
    })
}

fn lease_deadline(lease: Duration) -> DateTime<Utc> {
    Utc::now() + chrono::Duration::from_std(lease).unwrap_or_else(|_| chrono::Duration::seconds(30))
}

#[async_trait]
impl DurableActionLifecycle<DbErr> for SignalActionStore {
    async fn claim_by_id(
        &self,
        work_id: i64,
        node_id: &str,
        lease: Duration,
    ) -> Result<Option<ClaimedAction>, DbErr> {
        let token = Uuid::new_v4().to_string();
        let now = Utc::now();
        let result = agent_action_item::Entity::update_many()
            .col_expr(
                agent_action_item::Column::Status,
                Expr::value(STATUS_CLAIMED),
            )
            .col_expr(
                agent_action_item::Column::OwnerNode,
                Expr::value(Some(node_id.to_string())),
            )
            .col_expr(
                agent_action_item::Column::ClaimToken,
                Expr::value(Some(token.clone())),
            )
            .col_expr(
                agent_action_item::Column::Attempt,
                Expr::col(agent_action_item::Column::Attempt).add(1),
            )
            .col_expr(
                agent_action_item::Column::LeaseExpiresAt,
                Expr::value(Some(lease_deadline(lease))),
            )
            .col_expr(agent_action_item::Column::UpdatedAt, Expr::value(now))
            .filter(agent_action_item::Column::Id.eq(work_id))
            .filter(agent_action_item::Column::Status.eq(STATUS_APPROVED))
            .exec(&self.db)
            .await?;
        if result.rows_affected != 1 {
            return Ok(None);
        }
        self.row_for_claim_token(&token)
            .await?
            .map(claimed)
            .transpose()
    }

    async fn renew(&self, claim_token: &str, lease: Duration) -> Result<bool, DbErr> {
        let result = agent_action_item::Entity::update_many()
            .col_expr(
                agent_action_item::Column::LeaseExpiresAt,
                Expr::value(Some(lease_deadline(lease))),
            )
            .col_expr(
                agent_action_item::Column::UpdatedAt,
                Expr::value(Utc::now()),
            )
            .filter(agent_action_item::Column::ClaimToken.eq(claim_token))
            .filter(agent_action_item::Column::Status.is_in([STATUS_CLAIMED, STATUS_DISPATCHED]))
            .exec(&self.db)
            .await?;
        Ok(result.rows_affected == 1)
    }

    async fn mark_dispatched(&self, claim_token: &str, execution_id: &str) -> Result<bool, DbErr> {
        let Some(row) = self.row_for_claim_token(claim_token).await? else {
            return Ok(false);
        };
        let now = Utc::now();
        let result = agent_action_item::Entity::update_many()
            .col_expr(
                agent_action_item::Column::Status,
                Expr::value(STATUS_DISPATCHED),
            )
            .col_expr(
                agent_action_item::Column::ExecutionId,
                Expr::value(Some(execution_id.to_string())),
            )
            .col_expr(
                agent_action_item::Column::DispatchedAttempt,
                Expr::value(Some(row.attempt)),
            )
            .col_expr(
                agent_action_item::Column::DispatchIntentAt,
                Expr::value(Some(now)),
            )
            .col_expr(agent_action_item::Column::UpdatedAt, Expr::value(now))
            .filter(agent_action_item::Column::ClaimToken.eq(claim_token))
            .filter(agent_action_item::Column::Status.eq(STATUS_CLAIMED))
            .exec(&self.db)
            .await?;
        Ok(result.rows_affected == 1)
    }

    async fn rollback_unsent(&self, claim_token: &str, attempt: i32) -> Result<bool, DbErr> {
        let normal = agent_action_item::Entity::update_many()
            .col_expr(
                agent_action_item::Column::Status,
                Expr::value(STATUS_APPROVED),
            )
            .col_expr(
                agent_action_item::Column::ExecutionId,
                Expr::value(None::<String>),
            )
            .col_expr(
                agent_action_item::Column::DispatchedAttempt,
                Expr::value(None::<i32>),
            )
            .col_expr(
                agent_action_item::Column::DispatchIntentAt,
                Expr::value(None::<DateTime<Utc>>),
            )
            .col_expr(
                agent_action_item::Column::ClaimToken,
                Expr::value(None::<String>),
            )
            .col_expr(
                agent_action_item::Column::OwnerNode,
                Expr::value(None::<String>),
            )
            .col_expr(
                agent_action_item::Column::LeaseExpiresAt,
                Expr::value(None::<DateTime<Utc>>),
            )
            .col_expr(
                agent_action_item::Column::UpdatedAt,
                Expr::value(Utc::now()),
            )
            .filter(agent_action_item::Column::ClaimToken.eq(claim_token))
            .filter(agent_action_item::Column::Attempt.eq(attempt))
            .filter(agent_action_item::Column::Status.eq(STATUS_DISPATCHED))
            .filter(agent_action_item::Column::CancelRequestedAt.is_null())
            .exec(&self.db)
            .await?;
        if normal.rows_affected == 1 {
            return Ok(true);
        }
        let cancelled = agent_action_item::Entity::update_many()
            .col_expr(
                agent_action_item::Column::Status,
                Expr::value(STATUS_CANCELLED),
            )
            .col_expr(
                agent_action_item::Column::ExecutionId,
                Expr::value(None::<String>),
            )
            .col_expr(
                agent_action_item::Column::DispatchedAttempt,
                Expr::value(None::<i32>),
            )
            .col_expr(
                agent_action_item::Column::DispatchIntentAt,
                Expr::value(None::<DateTime<Utc>>),
            )
            .col_expr(
                agent_action_item::Column::ClaimToken,
                Expr::value(None::<String>),
            )
            .col_expr(
                agent_action_item::Column::OwnerNode,
                Expr::value(None::<String>),
            )
            .col_expr(
                agent_action_item::Column::LeaseExpiresAt,
                Expr::value(None::<DateTime<Utc>>),
            )
            .col_expr(
                agent_action_item::Column::ResultJson,
                Expr::value(Some(r#"{"cancelled":true}"#.to_string())),
            )
            .col_expr(
                agent_action_item::Column::ResultSchemaVersion,
                Expr::value(Some(1)),
            )
            .col_expr(
                agent_action_item::Column::UpdatedAt,
                Expr::value(Utc::now()),
            )
            .filter(agent_action_item::Column::ClaimToken.eq(claim_token))
            .filter(agent_action_item::Column::Attempt.eq(attempt))
            .filter(agent_action_item::Column::Status.eq(STATUS_DISPATCHED))
            .filter(agent_action_item::Column::CancelRequestedAt.is_not_null())
            .exec(&self.db)
            .await?;
        Ok(cancelled.rows_affected == 1)
    }

    async fn writeback(
        &self,
        claim_token: &str,
        attempt: i32,
        result: Value,
    ) -> Result<WritebackOutcome, DbErr> {
        let result_json = serde_json::to_string(&result)
            .map_err(|error| DbErr::Custom(format!("encode action result: {error}")))?;
        let updated = agent_action_item::Entity::update_many()
            .col_expr(agent_action_item::Column::Status, Expr::value(STATUS_DONE))
            .col_expr(
                agent_action_item::Column::ResultJson,
                Expr::value(Some(result_json)),
            )
            .col_expr(
                agent_action_item::Column::ResultSchemaVersion,
                Expr::value(Some(1)),
            )
            .col_expr(
                agent_action_item::Column::UpdatedAt,
                Expr::value(Utc::now()),
            )
            .filter(agent_action_item::Column::ClaimToken.eq(claim_token))
            .filter(agent_action_item::Column::Attempt.eq(attempt))
            .filter(agent_action_item::Column::Status.eq(STATUS_DISPATCHED))
            .exec(&self.db)
            .await?;
        if updated.rows_affected == 1 {
            return Ok(WritebackOutcome::Applied);
        }
        let existing = agent_action_item::Entity::find()
            .filter(agent_action_item::Column::ClaimToken.eq(claim_token))
            .filter(agent_action_item::Column::Attempt.eq(attempt))
            .one(&self.db)
            .await?;
        Ok(if existing.is_some_and(|row| row.status == STATUS_DONE) {
            WritebackOutcome::AlreadyResolved
        } else {
            WritebackOutcome::Stale
        })
    }

    async fn writeback_execution_result(
        &self,
        work_id: i64,
        execution_id: &str,
        attempt: i32,
        result: Value,
    ) -> Result<WritebackOutcome, DbErr> {
        let Some(row) = agent_action_item::Entity::find_by_id(work_id)
            .one(&self.db)
            .await?
        else {
            return Ok(WritebackOutcome::Stale);
        };
        if row.execution_id.as_deref() != Some(execution_id)
            || row.dispatched_attempt != Some(attempt)
        {
            return Ok(WritebackOutcome::Stale);
        }
        if row.status == STATUS_DONE || row.resolution.is_some() {
            return Ok(WritebackOutcome::AlreadyResolved);
        }
        if !matches!(row.status.as_str(), STATUS_DISPATCHED | STATUS_UNKNOWN) {
            return Ok(WritebackOutcome::Stale);
        }
        let result_json = serde_json::to_string(&result)
            .map_err(|error| DbErr::Custom(format!("encode action result: {error}")))?;
        let updated = agent_action_item::Entity::update_many()
            .col_expr(agent_action_item::Column::Status, Expr::value(STATUS_DONE))
            .col_expr(
                agent_action_item::Column::ResultJson,
                Expr::value(Some(result_json)),
            )
            .col_expr(
                agent_action_item::Column::ResultSchemaVersion,
                Expr::value(Some(1)),
            )
            .col_expr(
                agent_action_item::Column::Resolution,
                Expr::value(Some("late".to_string())),
            )
            .col_expr(
                agent_action_item::Column::UpdatedAt,
                Expr::value(Utc::now()),
            )
            .filter(agent_action_item::Column::Id.eq(work_id))
            .filter(agent_action_item::Column::ExecutionId.eq(execution_id))
            .filter(agent_action_item::Column::DispatchedAttempt.eq(attempt))
            .filter(agent_action_item::Column::Status.is_in([STATUS_DISPATCHED, STATUS_UNKNOWN]))
            .filter(agent_action_item::Column::Resolution.is_null())
            .exec(&self.db)
            .await?;
        Ok(if updated.rows_affected == 1 {
            WritebackOutcome::Applied
        } else {
            WritebackOutcome::AlreadyResolved
        })
    }

    async fn recover_expired(&self, kind: WorkKind) -> Result<Vec<RecoveredAction>, DbErr> {
        let now = Utc::now();
        let candidates = agent_action_item::Entity::find()
            .filter(agent_action_item::Column::Kind.eq(kind.as_str()))
            .filter(agent_action_item::Column::Status.is_in([STATUS_CLAIMED, STATUS_DISPATCHED]))
            .filter(agent_action_item::Column::LeaseExpiresAt.lt(now))
            .all(&self.db)
            .await?;
        let mut recovered = Vec::new();
        for row in candidates {
            let new_status = if row.status == STATUS_DISPATCHED && row.is_side_effecting {
                STATUS_UNKNOWN
            } else {
                STATUS_APPROVED
            };
            let mut update = agent_action_item::Entity::update_many()
                .col_expr(agent_action_item::Column::Status, Expr::value(new_status))
                .col_expr(
                    agent_action_item::Column::LeaseExpiresAt,
                    Expr::value(None::<DateTime<Utc>>),
                )
                .col_expr(agent_action_item::Column::UpdatedAt, Expr::value(now));
            if new_status == STATUS_APPROVED {
                update = update
                    .col_expr(
                        agent_action_item::Column::ClaimToken,
                        Expr::value(None::<String>),
                    )
                    .col_expr(
                        agent_action_item::Column::OwnerNode,
                        Expr::value(None::<String>),
                    )
                    .col_expr(
                        agent_action_item::Column::ExecutionId,
                        Expr::value(None::<String>),
                    )
                    .col_expr(
                        agent_action_item::Column::DispatchedAttempt,
                        Expr::value(None::<i32>),
                    )
                    .col_expr(
                        agent_action_item::Column::DispatchIntentAt,
                        Expr::value(None::<DateTime<Utc>>),
                    );
            }
            let updated = update
                .filter(agent_action_item::Column::Id.eq(row.id))
                .filter(agent_action_item::Column::Status.eq(row.status.clone()))
                .filter(agent_action_item::Column::LeaseExpiresAt.lt(now))
                .exec(&self.db)
                .await?;
            if updated.rows_affected == 1 {
                recovered.push(RecoveredAction {
                    work_id: row.id,
                    kind,
                    action_request_id: row.action_request_id,
                    exec_request_id: row.exec_request_id,
                    conversation_id: row.conversation_id,
                    tool_call_id: row.tool_call_id,
                    execution_id: (new_status == STATUS_UNKNOWN)
                        .then_some(row.execution_id)
                        .flatten(),
                    new_status: new_status.into(),
                });
            }
        }
        Ok(recovered)
    }

    async fn manual_resolve(&self, work_id: i64) -> Result<WritebackOutcome, DbErr> {
        let updated = agent_action_item::Entity::update_many()
            .col_expr(
                agent_action_item::Column::ManualResolvedAt,
                Expr::value(Some(Utc::now())),
            )
            .col_expr(
                agent_action_item::Column::UpdatedAt,
                Expr::value(Utc::now()),
            )
            .filter(agent_action_item::Column::Id.eq(work_id))
            .filter(agent_action_item::Column::Status.is_in([STATUS_DISPATCHED, STATUS_UNKNOWN]))
            .filter(agent_action_item::Column::ManualResolvedAt.is_null())
            .exec(&self.db)
            .await?;
        Ok(if updated.rows_affected == 1 {
            WritebackOutcome::Applied
        } else {
            WritebackOutcome::AlreadyResolved
        })
    }

    async fn request_cancel(
        &self,
        work_id: i64,
        requested_by: &str,
        cancelled_result_json: &str,
    ) -> Result<CancelRequestOutcome, DbErr> {
        let Some(row) = agent_action_item::Entity::find_by_id(work_id)
            .one(&self.db)
            .await?
        else {
            return Ok(CancelRequestOutcome::NotFound);
        };
        if matches!(
            row.status.as_str(),
            STATUS_DONE | STATUS_REJECTED | STATUS_EXPIRED | STATUS_CANCELLED
        ) {
            return Ok(CancelRequestOutcome::AlreadyTerminal);
        }
        let now = Utc::now();
        if matches!(row.status.as_str(), STATUS_DISPATCHED | STATUS_UNKNOWN) {
            agent_action_item::Entity::update_many()
                .col_expr(
                    agent_action_item::Column::CancelRequestedAt,
                    Expr::value(Some(now)),
                )
                .col_expr(
                    agent_action_item::Column::CancelRequestedBy,
                    Expr::value(Some(requested_by.to_string())),
                )
                .col_expr(
                    agent_action_item::Column::CancelGeneration,
                    Expr::value(row.execution_id.clone()),
                )
                .col_expr(agent_action_item::Column::UpdatedAt, Expr::value(now))
                .filter(agent_action_item::Column::Id.eq(work_id))
                .exec(&self.db)
                .await?;
            return Ok(CancelRequestOutcome::Requested {
                execution_id: row.execution_id,
            });
        }
        let updated = agent_action_item::Entity::update_many()
            .col_expr(
                agent_action_item::Column::Status,
                Expr::value(STATUS_CANCELLED),
            )
            .col_expr(
                agent_action_item::Column::ResultJson,
                Expr::value(Some(cancelled_result_json.to_string())),
            )
            .col_expr(
                agent_action_item::Column::ResultSchemaVersion,
                Expr::value(Some(1)),
            )
            .col_expr(
                agent_action_item::Column::CancelRequestedAt,
                Expr::value(Some(now)),
            )
            .col_expr(
                agent_action_item::Column::CancelRequestedBy,
                Expr::value(Some(requested_by.to_string())),
            )
            .col_expr(agent_action_item::Column::UpdatedAt, Expr::value(now))
            .filter(agent_action_item::Column::Id.eq(work_id))
            .filter(agent_action_item::Column::Status.is_in([
                STATUS_AWAITING_APPROVAL,
                STATUS_APPROVED,
                STATUS_CLAIMED,
            ]))
            .exec(&self.db)
            .await?;
        Ok(if updated.rows_affected == 1 {
            CancelRequestOutcome::ConvergedNotExecuted
        } else {
            CancelRequestOutcome::AlreadyTerminal
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::{ConnectionTrait, Database, Schema};

    async fn store() -> SignalActionStore {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        let schema = Schema::new(db.get_database_backend());
        db.execute(&schema.create_table_from_entity(agent_action_item::Entity))
            .await
            .unwrap();
        SignalActionStore::new(db)
    }

    fn draft(id: &str) -> PersonalActionDraft {
        PersonalActionDraft {
            kind: WorkKind::ComputerAction,
            action_request_id: id.into(),
            conversation_id: "conv-1".into(),
            turn_id: "turn-1".into(),
            tool_call_id: "call-1".into(),
            owner_actor_id: "owner-1".into(),
            target_device_id: "device-1".into(),
            policy_revision: 7,
            draft_hash: "sha256:test".into(),
            payload_json: r#"{"schema":"computer_action/v1"}"#.into(),
            payload_schema_version: 1,
            is_side_effecting: true,
        }
    }

    #[tokio::test]
    async fn personal_approval_and_concurrent_claim_are_owner_fenced() {
        let store = store().await;
        let row = store
            .create_pending(draft("action-1"), Duration::from_secs(60))
            .await
            .unwrap();
        assert_eq!(row.exec_request_id, None);
        assert_eq!(
            store
                .resolve_personal(row.id, "org-member", "device-1", true)
                .await
                .unwrap(),
            ApprovalOutcome::SubjectMismatch
        );
        assert_eq!(
            store
                .resolve_personal(row.id, "owner-1", "device-1", true)
                .await
                .unwrap(),
            ApprovalOutcome::Approved
        );

        let (left, right) = tokio::join!(
            store.claim_personal_by_id(
                row.id,
                "owner-1",
                "device-1",
                "node-a",
                Duration::from_secs(30),
            ),
            store.claim_personal_by_id(
                row.id,
                "owner-1",
                "device-1",
                "node-b",
                Duration::from_secs(30),
            )
        );
        let winners = [left.unwrap(), right.unwrap()]
            .into_iter()
            .filter(Option::is_some)
            .count();
        assert_eq!(winners, 1);
    }

    #[tokio::test]
    async fn crash_manual_disposition_and_late_result_preserve_execution_fact() {
        let store = store().await;
        let row = store
            .create_pending(draft("action-2"), Duration::from_secs(60))
            .await
            .unwrap();
        store
            .resolve_personal(row.id, "owner-1", "device-1", true)
            .await
            .unwrap();
        let claim = store
            .claim_personal_by_id(
                row.id,
                "owner-1",
                "device-1",
                "node-a",
                Duration::from_secs(30),
            )
            .await
            .unwrap()
            .unwrap();
        assert!(
            store
                .mark_dispatched(&claim.claim_token, "generation-1")
                .await
                .unwrap()
        );
        agent_action_item::Entity::update_many()
            .col_expr(
                agent_action_item::Column::LeaseExpiresAt,
                Expr::value(Some(Utc::now() - chrono::Duration::seconds(1))),
            )
            .filter(agent_action_item::Column::Id.eq(row.id))
            .exec(&store.db)
            .await
            .unwrap();

        let recovered = store
            .recover_expired(WorkKind::ComputerAction)
            .await
            .unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].new_status, STATUS_UNKNOWN);
        assert_eq!(
            store
                .manually_dispose_for_subject(
                    row.id,
                    "generation-1",
                    "conv-1",
                    "other-owner",
                    "device-1",
                )
                .await
                .unwrap(),
            ManualDispositionOutcome::SubjectMismatch
        );
        assert_eq!(
            store
                .manually_dispose_for_subject(
                    row.id,
                    "generation-1",
                    "conv-1",
                    "owner-1",
                    "device-1",
                )
                .await
                .unwrap(),
            ManualDispositionOutcome::Applied
        );
        assert_eq!(
            store
                .manually_dispose_for_subject(
                    row.id,
                    "generation-1",
                    "conv-1",
                    "owner-1",
                    "device-1",
                )
                .await
                .unwrap(),
            ManualDispositionOutcome::AlreadyResolved
        );
        assert_eq!(
            store
                .writeback_execution_result(
                    row.id,
                    "generation-1",
                    claim.attempt,
                    serde_json::json!({"completed": true}),
                )
                .await
                .unwrap(),
            WritebackOutcome::Applied
        );
        let final_row = agent_action_item::Entity::find_by_id(row.id)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(final_row.status, STATUS_DONE);
        assert_eq!(final_row.resolution.as_deref(), Some("late"));
        assert!(final_row.manual_resolved_at.is_some());
        assert!(final_row.result_json.is_some());
    }

    #[tokio::test]
    async fn cancel_before_dispatch_converges_as_definitely_not_executed() {
        let store = store().await;
        let row = store
            .create_pending(draft("action-3"), Duration::from_secs(60))
            .await
            .unwrap();
        assert_eq!(
            store
                .request_cancel(row.id, "owner-1", r#"{"cancelled":true}"#)
                .await
                .unwrap(),
            CancelRequestOutcome::ConvergedNotExecuted
        );
        let row = agent_action_item::Entity::find_by_id(row.id)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, STATUS_CANCELLED);
        assert!(row.execution_id.is_none());
    }

    #[tokio::test]
    async fn cancel_after_dispatch_and_definitely_unsent_converges_cancelled() {
        let store = store().await;
        let row = store
            .create_pending(draft("action-4"), Duration::from_secs(60))
            .await
            .unwrap();
        store
            .resolve_personal(row.id, "owner-1", "device-1", true)
            .await
            .unwrap();
        let claim = store
            .claim_personal_by_id(
                row.id,
                "owner-1",
                "device-1",
                "node-a",
                Duration::from_secs(30),
            )
            .await
            .unwrap()
            .unwrap();
        assert!(
            store
                .mark_dispatched(&claim.claim_token, "generation-unsent")
                .await
                .unwrap()
        );
        assert_eq!(
            store
                .request_cancel(row.id, "owner-1", r#"{"cancelled":true}"#)
                .await
                .unwrap(),
            CancelRequestOutcome::Requested {
                execution_id: Some("generation-unsent".into()),
            }
        );
        assert!(
            store
                .rollback_unsent(&claim.claim_token, claim.attempt)
                .await
                .unwrap()
        );
        let row = agent_action_item::Entity::find_by_id(row.id)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, STATUS_CANCELLED);
        assert!(row.execution_id.is_none());
        assert!(row.result_json.is_some());
    }
}
