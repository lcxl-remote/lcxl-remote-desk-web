//! Durable call-cost reservations; control receipts use a separate operation kind.
use super::*;
use crate::config::connection::DatabaseTransaction;
use crate::entity::agent_delegation_reservation as reservation_row;
use desk_diagnose_core::subagent::{
    budget::Usage,
    reservation::{DelegationCallKind, DelegationCallReservation, ReviewCallAuthority},
};
use sea_orm::ActiveModelTrait;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BudgetAdmission {
    Exhausted,
    Reserved(DelegationCallReservation),
    /// A replay cannot admit another physical call. The host retrieves the
    /// original call ledger/result or creates a separately charged retry identity.
    AlreadyReserved(DelegationCallReservation),
    Settled(DelegationCallReservation, Usage),
}

pub(super) fn decode_reservation(
    row: &reservation_row::Model,
) -> Result<DelegationCallReservation, DbErr> {
    let reservation: DelegationCallReservation =
        serde_json::from_str(&row.reservation_json).map_err(|_| invalid())?;
    reservation.validate().map_err(|_| invalid())?;
    if row.reservation_id != reservation.reservation_id
        || row.root_conversation_id != reservation.root_conversation_id
        || row.group_id != reservation.group_id
        || row.conversation_id != reservation.conversation_id
        || row.task_id != reservation.task_id
        || row.operation_kind != reservation.kind.as_str()
        || row.arguments_sha256 != reservation.arguments_sha256
        || i64::try_from(reservation.source_epoch).ok() != Some(row.source_epoch)
        || i64::try_from(reservation.input_revision).ok() != Some(row.input_revision)
        || i64::try_from(reservation.control_revision).ok() != Some(row.control_revision)
        || reservation
            .planning_lease_token
            .map(i64::try_from)
            .transpose()
            .map_err(|_| invalid())?
            != row.planning_lease_token
        || !matches!(row.state.as_str(), "reserved" | "usage_unknown" | "settled")
        || (row.state == "settled") != row.actual_json.is_some()
        || (row.state == "settled") != row.settled_at.is_some()
        || row.provider_receipt_id.is_some() != row.provider_receipt_kind.is_some()
        || row.provider_receipt_id.is_some() != row.provider_started_at_ms.is_some()
        || row
            .provider_receipt_id
            .as_ref()
            .is_some_and(|id| id.is_empty() || id.len() > 256)
        || row.provider_started_at_ms.is_some_and(|at| at <= 0)
        || row
            .provider_receipt_kind
            .as_ref()
            .is_some_and(|kind| !matches!(kind.as_str(), "manager_ai_call" | "oss_model_egress"))
        || (reservation.kind == DelegationCallKind::Tool && row.provider_receipt_id.is_some())
        || row
            .source_schedule_budget_id
            .as_ref()
            .is_some_and(|id| id.len() != 64 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()))
        || (reservation.kind == DelegationCallKind::Tool && row.source_schedule_budget_id.is_some())
        || row.version <= 0
    {
        return Err(invalid());
    }
    Ok(reservation)
}

pub(crate) async fn replace_group_on<C: ConnectionTrait + crate::config::ConfigConnection>(
    db: &C,
    old: &group_row::Model,
    group: &DelegationGroup,
    now_ms: i64,
) -> Result<(), DbErr> {
    group.validate().map_err(|_| invalid())?;
    let changed = group_row::Entity::update_many()
        .set(group_row::ActiveModel {
            source_admission: Set(admission_label(group.source_admission).into()),
            source_epoch: Set(i64::try_from(group.source_epoch).map_err(|_| invalid())?),
            parent_input_revision: Set(
                i64::try_from(group.parent_input_revision).map_err(|_| invalid())?
            ),
            parent_control_revision: Set(
                i64::try_from(group.parent_control_revision).map_err(|_| invalid())?
            ),
            parent_active: Set(group.parent_active),
            state_json: Set(serde_json::to_string(group).map_err(|_| invalid())?),
            version: Set(i64::try_from(group.version).map_err(|_| invalid())?),
            updated_at: Set(now_ms),
            ..Default::default()
        })
        .filter(group_row::Column::Id.eq(old.id))
        .filter(group_row::Column::Version.eq(old.version))
        .exec(db)
        .await?;
    if changed.rows_affected != 1 {
        return Err(invalid());
    }
    Ok(())
}

/// Join this operation to the goal/source reservation transaction. The caller
/// holds owner/root and the sorted child controls before reading these rows.
#[cfg(test)]
pub(crate) async fn reserve_budget_on(
    txn: &DatabaseTransaction,
    session: &PersistedAgentSession,
    logical_call_id: &str,
    kind: DelegationCallKind,
    arguments_sha256: &str,
    upper: Usage,
    now_ms: i64,
) -> Result<BudgetAdmission, DbErr> {
    reserve_budget_with_authority_on(
        txn,
        session,
        logical_call_id,
        kind,
        arguments_sha256,
        upper,
        None,
        now_ms,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn reserve_budget_with_authority_on(
    txn: &DatabaseTransaction,
    session: &PersistedAgentSession,
    logical_call_id: &str,
    kind: DelegationCallKind,
    arguments_sha256: &str,
    upper: Usage,
    review_authority: Option<&ReviewCallAuthority>,
    now_ms: i64,
) -> Result<BudgetAdmission, DbErr> {
    let group_id = session.delegation_group_id.as_deref().ok_or_else(invalid)?;
    let root = session
        .agent_role
        .binding()
        .map_or(session.conversation_id.as_str(), |binding| {
            binding.root_conversation_id.as_str()
        });
    let row = group_row::Entity::find()
        .filter(group_row::Column::GroupId.eq(group_id))
        .filter(group_row::Column::RootConversationId.eq(root))
        .filter(group_row::Column::ActorId.eq(&session.actor_id))
        .filter(group_row::Column::DeviceId.eq(&session.device_id))
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    let mut group = decode_group(&row)?;
    let child = session.agent_role.binding();
    let logical_key = format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&(
                root,
                group_id,
                &session.conversation_id,
                "delegation_budget",
                logical_call_id,
            ))
            .map_err(|_| invalid())?
        )
    );
    let reservation = DelegationCallReservation {
        reservation_id: format!("delegation-call-{logical_key}"),
        logical_call_id: logical_call_id.into(),
        root_conversation_id: root.into(),
        group_id: group_id.into(),
        conversation_id: session.conversation_id.clone(),
        task_id: child.map(|binding| binding.task_id.clone()),
        kind,
        arguments_sha256: arguments_sha256.into(),
        source_epoch: group.source_epoch,
        input_revision: session.input_revision,
        control_revision: session.control_revision,
        planning_lease_token: review_authority.is_none().then_some(session.lease_token),
        review_authority: review_authority.cloned(),
        upper,
        source_goal_upper: None,
    };
    reservation.validate().map_err(|_| invalid())?;
    if let Some(old) = reservation_row::Entity::find()
        .filter(reservation_row::Column::LogicalKeySha256.eq(&logical_key))
        .one(txn)
        .await?
    {
        let existing = decode_reservation(&old)?;
        if existing.arguments_sha256 != reservation.arguments_sha256
            || existing.kind != reservation.kind
            || existing.upper != reservation.upper
            || existing.input_revision != reservation.input_revision
            || existing.control_revision != reservation.control_revision
            || existing.task_id != reservation.task_id
            || existing.group_id != reservation.group_id
            || existing.root_conversation_id != reservation.root_conversation_id
            || existing.conversation_id != reservation.conversation_id
            || existing.logical_call_id != reservation.logical_call_id
            || existing.review_authority != reservation.review_authority
        {
            return Err(invalid());
        }
        return if old.state == "settled" {
            Ok(BudgetAdmission::Settled(
                existing,
                serde_json::from_str(old.actual_json.as_deref().ok_or_else(invalid)?)
                    .map_err(|_| invalid())?,
            ))
        } else {
            Ok(BudgetAdmission::AlreadyReserved(existing))
        };
    }
    if session.surface != AgentSessionSurface::AiAssistant
        || (review_authority.is_none() && !session.turn_state.is_active())
        || group.source_admission != SourceAdmission::Open
    {
        return Err(invalid());
    }
    let live = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq(&session.conversation_id))
        .filter(session_row::Column::ActorId.eq(&session.actor_id))
        .filter(session_row::Column::DeviceId.eq(&session.device_id))
        .filter(session_row::Column::Version.eq(session.version))
        .filter(
            session_row::Column::LeaseToken
                .eq(i64::try_from(session.lease_token).map_err(|_| invalid())?),
        )
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    if review_authority.is_none()
        && live
            .lease_deadline
            .is_none_or(|deadline| deadline.timestamp_millis() <= now_ms)
    {
        return Err(invalid());
    }
    let durable = PersistedAgentSession::decode_json(&live.state_json).map_err(|_| invalid())?;
    if durable.agent_role != session.agent_role
        || durable.input_revision != session.input_revision
        || durable.control_revision != session.control_revision
        || (review_authority.is_none() && !durable.turn_state.is_active())
    {
        return Err(invalid());
    }
    if let Some(review) = review_authority {
        super::review_budget::validate_review_lease_on(txn, review, now_ms).await?;
        if logical_call_id != format!("review:{}", review.candidate_id)
            || arguments_sha256 != review.request_sha256
        {
            return Err(invalid());
        }
        if !durable.allows_delegated_review() || durable.main_stopped {
            return Err(invalid());
        }
    }
    if let Some(binding) = child {
        let task_row = run_row::Entity::find()
            .filter(run_row::Column::TaskId.eq(&binding.task_id))
            .filter(run_row::Column::RootConversationId.eq(root))
            .filter(run_row::Column::ActorId.eq(&session.actor_id))
            .filter(run_row::Column::DeviceId.eq(&session.device_id))
            .one(txn)
            .await?
            .ok_or_else(invalid)?;
        let run = decode_run(&task_row)?;
        run.validate_session(session).map_err(|_| invalid())?;
        run.require_current(run.fence()).map_err(|_| invalid())?;
        if binding.source_epoch != group.source_epoch
            || binding.source != group.source
            || now_ms >= binding.deadline_ms
        {
            return Err(invalid());
        }
    } else if !group.can_interpret(session.input_revision, session.control_revision) {
        return Err(invalid());
    }
    if group
        .budget
        .reserve(group.limits, upper, child.is_some(), now_ms)
        .is_err()
    {
        return Ok(BudgetAdmission::Exhausted);
    }
    group.version = group.version.checked_add(1).ok_or_else(invalid)?;
    replace_group_on(txn, &row, &group, now_ms).await?;
    reservation_row::ActiveModel {
        reservation_id: Set(reservation.reservation_id.clone()),
        logical_key_sha256: Set(logical_key),
        root_conversation_id: Set(root.into()),
        group_id: Set(group_id.into()),
        conversation_id: Set(session.conversation_id.clone()),
        task_id: Set(reservation.task_id.clone()),
        operation_kind: Set(kind.as_str().into()),
        arguments_sha256: Set(arguments_sha256.into()),
        source_epoch: Set(i64::try_from(group.source_epoch).map_err(|_| invalid())?),
        input_revision: Set(i64::try_from(session.input_revision).map_err(|_| invalid())?),
        control_revision: Set(i64::try_from(session.control_revision).map_err(|_| invalid())?),
        planning_lease_token: Set(reservation
            .planning_lease_token
            .map(i64::try_from)
            .transpose()
            .map_err(|_| invalid())?),
        reservation_json: Set(serde_json::to_string(&reservation).map_err(|_| invalid())?),
        actual_json: Set(None),
        state: Set("reserved".into()),
        version: Set(1),
        created_at: Set(now_ms),
        settled_at: Set(None),
        ..Default::default()
    }
    .insert(txn)
    .await?;
    Ok(BudgetAdmission::Reserved(reservation))
}

/// Settlement records admitted usage after source pause/cancel or lease change;
/// it provides no planning authority. Unknown usage remains outstanding.
pub(crate) async fn settle_budget_on(
    txn: &DatabaseTransaction,
    reservation: &DelegationCallReservation,
    actual: Option<Usage>,
    now_ms: i64,
) -> Result<(), DbErr> {
    reservation.validate().map_err(|_| invalid())?;
    let row = reservation_row::Entity::find()
        .filter(reservation_row::Column::ReservationId.eq(&reservation.reservation_id))
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    if decode_reservation(&row)? != *reservation {
        return Err(invalid());
    }
    if row.state == "settled" {
        let settled: Usage = serde_json::from_str(row.actual_json.as_deref().ok_or_else(invalid)?)
            .map_err(|_| invalid())?;
        return if Some(settled) == actual {
            Ok(())
        } else {
            Err(invalid())
        };
    }
    if actual.is_none() && row.state == "usage_unknown" {
        return Ok(());
    }
    if let Some(actual) = actual {
        let unstarted_model = actual == Usage::default()
            && reservation.kind != DelegationCallKind::Tool
            && row.provider_receipt_id.is_none();
        if (!unstarted_model
            && (actual.model_calls != reservation.upper.model_calls
                || actual.tool_calls != reservation.upper.tool_calls))
            || (reservation.kind == DelegationCallKind::Tool && actual.tokens != 0)
        {
            return Err(invalid());
        }
        let group_row = group_row::Entity::find()
            .filter(group_row::Column::GroupId.eq(&reservation.group_id))
            .filter(group_row::Column::RootConversationId.eq(&reservation.root_conversation_id))
            .one(txn)
            .await?
            .ok_or_else(invalid)?;
        let mut group = decode_group(&group_row)?;
        group
            .budget
            .settle(reservation.upper, actual, reservation.task_id.is_some())
            .map_err(|_| invalid())?;
        group.version = group.version.checked_add(1).ok_or_else(invalid)?;
        replace_group_on(txn, &group_row, &group, now_ms).await?;
    }
    let changed = reservation_row::Entity::update_many()
        .set(reservation_row::ActiveModel {
            state: Set(if actual.is_some() {
                "settled"
            } else {
                "usage_unknown"
            }
            .into()),
            actual_json: Set(actual
                .map(|usage| serde_json::to_string(&usage))
                .transpose()
                .map_err(|_| invalid())?),
            settled_at: Set(actual.map(|_| now_ms)),
            version: Set(row.version.checked_add(1).ok_or_else(invalid)?),
            ..Default::default()
        })
        .filter(reservation_row::Column::Id.eq(row.id))
        .filter(reservation_row::Column::Version.eq(row.version))
        .exec(txn)
        .await?;
    if changed.rows_affected != 1 {
        return Err(invalid());
    }
    Ok(())
}
