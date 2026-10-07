//! Independent reviewer leases share the original source allocation, not a planner lease.
use super::*;
use crate::entity::agent_approval_review as review_row;
use desk_diagnose_core::{
    approval_review::{APPROVAL_REVIEW_STATUS_REVIEWING, ApprovalReviewCandidate},
    goal::GoalUsage,
    subagent::{
        budget::Usage,
        reservation::{DelegationCallKind, DelegationCallReservation, ReviewCallAuthority},
    },
};
use sea_orm::DatabaseTransaction;

pub(crate) async fn validate_review_lease_on(
    txn: &DatabaseTransaction,
    authority: &ReviewCallAuthority,
    now_ms: i64,
) -> Result<(), DbErr> {
    authority.validate().map_err(|_| invalid())?;
    let row = review_row::Entity::find()
        .filter(review_row::Column::CandidateId.eq(&authority.candidate_id))
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    let frozen: ReviewCallAuthority =
        serde_json::from_str(row.call_authority_json.as_deref().ok_or_else(invalid)?)
            .map_err(|_| invalid())?;
    if frozen != *authority
        || row.conversation_id != authority.conversation_id
        || row.actor_id != authority.actor_id
        || row.device_id != authority.device_id
        || row.status != APPROVAL_REVIEW_STATUS_REVIEWING
        || row.lease_epoch != authority.lease_epoch as i64
        || row.lease_owner.as_deref() != Some(authority.lease_owner.as_str())
        || row.lease_deadline != Some(authority.lease_deadline_ms as i64)
        || row.model_config_revision != authority.model_config_revision as i64
        || row.context_hmac_sha256 != authority.context_hmac_sha256
        || row.delegation_id != authority.delegation_id
        || row.source_id != authority.source_record_id
        || now_ms < 0
        || now_ms as u64 >= authority.lease_deadline_ms
        || now_ms >= row.expires_at
        || row.provider_receipt_id.is_some()
        || row.provider_receipt_kind.is_some()
        || row.provider_started_at_ms.is_some()
    {
        return Err(invalid());
    }
    Ok(())
}

pub(crate) async fn validate_review_scope_on(
    txn: &DatabaseTransaction,
    authority: &ReviewCallAuthority,
    now_ms: i64,
) -> Result<(), DbErr> {
    validate_review_lease_on(txn, authority, now_ms).await?;
    let configured = crate::approval_model_provider::load(txn).await?;
    if configured.unavailable_reason().is_some()
        || u64::try_from(configured.configuration_revision).ok()
            != Some(authority.model_config_revision)
        || configured.destination_identity().ok().as_ref() != Some(&authority.model_destination)
    {
        return Err(invalid());
    }
    let row = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq(&authority.conversation_id))
        .filter(session_row::Column::ActorId.eq(&authority.actor_id))
        .filter(session_row::Column::DeviceId.eq(&authority.device_id))
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    let session = PersistedAgentSession::decode_json(&row.state_json).map_err(|_| invalid())?;
    if session.surface != AgentSessionSurface::AiAssistant
        || session.main_stopped
        || session.input_revision != authority.input_revision
        || session.control_revision != authority.control_revision
        || session.delegation_group_id != authority.group_id
        || !session.allows_delegated_review()
        || u64::try_from(session.policy_revision).ok() != Some(authority.policy_revision)
        || session.agent_role.binding().map(|binding| &binding.task_id)
            != authority.task_id.as_ref()
    {
        return Err(invalid());
    }
    let delegated = crate::entity::agent_approval_delegation::Entity::find()
        .filter(
            crate::entity::agent_approval_delegation::Column::DelegationId
                .eq(&authority.delegation_id),
        )
        .filter(
            crate::entity::agent_approval_delegation::Column::ConversationId
                .eq(&authority.conversation_id),
        )
        .filter(crate::entity::agent_approval_delegation::Column::ActorId.eq(&authority.actor_id))
        .filter(crate::entity::agent_approval_delegation::Column::DeviceId.eq(&authority.device_id))
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    let delegated = crate::agent_approval_store::decode(&delegated)?;
    delegated
        .require_current(
            &authority.conversation_id,
            &authority.actor_id,
            &authority.device_id,
        )
        .map_err(|_| invalid())?;
    if delegated.revision != authority.delegation_revision {
        return Err(invalid());
    }
    validate_review_subject_on(txn, authority, &row, &session, now_ms).await?;
    let Some(group_id) = &authority.group_id else {
        return if session.agent_role.is_main() {
            Ok(())
        } else {
            Err(invalid())
        };
    };
    let source = group_row::Entity::find()
        .filter(group_row::Column::GroupId.eq(group_id))
        .filter(group_row::Column::RootConversationId.eq(&authority.root_conversation_id))
        .filter(group_row::Column::ActorId.eq(&authority.actor_id))
        .filter(group_row::Column::DeviceId.eq(&authority.device_id))
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    let group = decode_group(&source)?;
    if group.source_admission != SourceAdmission::Open
        || Some(group.source_epoch) != authority.source_epoch
        || now_ms >= group.limits.deadline_ms
    {
        return Err(invalid());
    }
    parent_on(
        txn,
        &group.root_conversation_id,
        &group.actor_id,
        &group.device_id,
    )
    .await?;
    if let Some(task_id) = &authority.task_id {
        let task = run_row::Entity::find()
            .filter(run_row::Column::TaskId.eq(task_id))
            .filter(run_row::Column::GroupId.eq(group_id))
            .filter(run_row::Column::ActorId.eq(&authority.actor_id))
            .filter(run_row::Column::DeviceId.eq(&authority.device_id))
            .one(txn)
            .await?
            .ok_or_else(invalid)?;
        let run = decode_run(&task)?;
        run.validate_session(&session).map_err(|_| invalid())?;
        run.require_current(run.fence()).map_err(|_| invalid())?;
        if run.binding.source_epoch != group.source_epoch
            || run.binding.source != group.source
            || now_ms >= run.binding.deadline_ms
        {
            return Err(invalid());
        }
    } else if !group.can_interpret(session.input_revision, session.control_revision) {
        return Err(invalid());
    }
    Ok(())
}

/// Called after inserting the unique review row, before committing its claim.
/// Failure rolls back review quota, group/goal quota and the claim together.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn reserve_review_call_on(
    txn: &DatabaseTransaction,
    candidate: &ApprovalReviewCandidate,
    destination: &desk_agent_protocol::data_lineage::DestinationIdentity,
    authorized: &desk_diagnose_core::approval_egress::AuthorizedApprovalReview,
    prices: desk_diagnose_core::approval_cost::ApprovalTokenPrices,
    reserved_tokens: u64,
    now_ms: i64,
) -> Result<(ReviewCallAuthority, Option<DelegationCallReservation>), DbErr> {
    let review = review_row::Entity::find()
        .filter(review_row::Column::CandidateId.eq(&candidate.candidate_id))
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    if review.call_authority_json.is_some() || review.delegation_reservation_id.is_some() {
        return Err(invalid());
    }
    let row = session_row::Entity::find()
        .filter(session_row::Column::ConversationId.eq(&candidate.conversation_id))
        .filter(session_row::Column::ActorId.eq(&candidate.owner_id))
        .filter(session_row::Column::DeviceId.eq(&candidate.device_id))
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    let mut session = PersistedAgentSession::decode_json(&row.state_json).map_err(|_| invalid())?;
    session.version = row.version;
    if session.input_revision != candidate.input_revision {
        return Err(invalid());
    }
    let root = session
        .agent_role
        .binding()
        .map_or(session.conversation_id.as_str(), |binding| {
            binding.root_conversation_id.as_str()
        });
    let group = if let Some(group_id) = &session.delegation_group_id {
        let row = group_row::Entity::find()
            .filter(group_row::Column::GroupId.eq(group_id))
            .filter(group_row::Column::RootConversationId.eq(root))
            .filter(group_row::Column::ActorId.eq(&candidate.owner_id))
            .filter(group_row::Column::DeviceId.eq(&candidate.device_id))
            .one(txn)
            .await?
            .ok_or_else(invalid)?;
        Some(decode_group(&row)?)
    } else {
        None
    };
    let request = authorized.model_request(candidate).map_err(|_| invalid())?;
    let request_sha256 = desk_diagnose_core::approval_review::reviewer_request_digest(&request)
        .map_err(|_| invalid())?;
    let authority = ReviewCallAuthority {
        candidate_id: candidate.candidate_id.clone(),
        lease_epoch: u64::try_from(review.lease_epoch).map_err(|_| invalid())?,
        lease_owner: review.lease_owner.clone().ok_or_else(invalid)?,
        lease_deadline_ms: review
            .lease_deadline
            .map(u64::try_from)
            .transpose()
            .map_err(|_| invalid())?
            .ok_or_else(invalid)?,
        model_config_revision: u64::try_from(review.model_config_revision)
            .map_err(|_| invalid())?,
        delegation_id: candidate.delegation_id.clone(),
        delegation_revision: candidate.delegation_revision,
        policy_revision: candidate.policy_revision,
        source: candidate.source.clone(),
        source_record_id: review.source_id.clone(),
        source_planning_lease_token: matches!(
            candidate.source,
            desk_diagnose_core::approval_review::ApprovalSource::ConcreteCall { .. }
        )
        .then_some(session.lease_token),
        context_hmac_sha256: review.context_hmac_sha256.clone(),
        request_sha256,
        model_destination: destination.clone(),
        root_conversation_id: root.into(),
        group_id: session.delegation_group_id.clone(),
        task_id: session
            .agent_role
            .binding()
            .map(|binding| binding.task_id.clone()),
        conversation_id: session.conversation_id.clone(),
        actor_id: session.actor_id.clone(),
        device_id: session.device_id.clone(),
        input_revision: session.input_revision,
        control_revision: session.control_revision,
        source_epoch: group.as_ref().map(|group| group.source_epoch),
    };
    authority.validate().map_err(|_| invalid())?;
    let changed = review_row::Entity::update_many()
        .set(review_row::ActiveModel {
            call_authority_json: Set(Some(
                serde_json::to_string(&authority).map_err(|_| invalid())?,
            )),
            token_prices_json: Set(Some(
                serde_json::to_string(&prices.validate().ok_or_else(invalid)?)
                    .map_err(|_| invalid())?,
            )),
            ..Default::default()
        })
        .filter(review_row::Column::Id.eq(review.id))
        .filter(review_row::Column::Status.eq(APPROVAL_REVIEW_STATUS_REVIEWING))
        .filter(review_row::Column::CallAuthorityJson.is_null())
        .exec(txn)
        .await?;
    if changed.rows_affected != 1 {
        return Err(invalid());
    }
    validate_review_scope_on(txn, &authority, now_ms).await?;
    let Some(group) = group else {
        return Ok((authority, None));
    };
    let digest = authority.request_sha256.clone();
    let upper = Usage {
        model_calls: 1,
        tool_calls: 0,
        tokens: reserved_tokens,
    };
    let goal_upper = GoalUsage {
        input_tokens: reserved_tokens.checked_sub(2048).ok_or_else(invalid)?,
        output_tokens: 2048,
        model_calls: 1,
        active_time_ms: authority.lease_deadline_ms.saturating_sub(now_ms as u64),
        ..Default::default()
    };
    let admission = funding::reserve_call_with_authority_on(
        txn,
        &session,
        &format!("review:{}", candidate.candidate_id),
        DelegationCallKind::ApprovalReview,
        &digest,
        upper,
        group.source.goal_id().map(|_| goal_upper),
        Some(&authority),
        now_ms,
    )
    .await?;
    let BudgetAdmission::Reserved(reservation) = admission else {
        return Err(invalid());
    };
    let changed = review_row::Entity::update_many()
        .set(review_row::ActiveModel {
            delegation_reservation_id: Set(Some(reservation.reservation_id.clone())),
            ..Default::default()
        })
        .filter(review_row::Column::Id.eq(review.id))
        .filter(review_row::Column::DelegationReservationId.is_null())
        .filter(review_row::Column::ProviderReceiptId.is_null())
        .exec(txn)
        .await?;
    if changed.rows_affected != 1 {
        return Err(invalid());
    }
    Ok((authority, Some(reservation)))
}

/// Shares the provider-start transaction. Replays can never authorize a second dial.
pub(crate) async fn mark_review_dispatch_on(
    txn: &DatabaseTransaction,
    authority: &ReviewCallAuthority,
    receipt_kind: &str,
    receipt_id: &str,
    now_ms: i64,
) -> Result<(), DbErr> {
    validate_review_scope_on(txn, authority, now_ms).await?;
    if !matches!(receipt_kind, "manager_ai_call" | "oss_model_egress")
        || receipt_id.is_empty()
        || receipt_id.len() > 256
    {
        return Err(invalid());
    }
    let changed = review_row::Entity::update_many()
        .set(review_row::ActiveModel {
            provider_receipt_kind: Set(Some(receipt_kind.into())),
            provider_receipt_id: Set(Some(receipt_id.into())),
            provider_started_at_ms: Set(Some(now_ms)),
            ..Default::default()
        })
        .filter(review_row::Column::CandidateId.eq(&authority.candidate_id))
        .filter(review_row::Column::Status.eq(APPROVAL_REVIEW_STATUS_REVIEWING))
        .filter(review_row::Column::LeaseEpoch.eq(authority.lease_epoch as i64))
        .filter(review_row::Column::LeaseOwner.eq(&authority.lease_owner))
        .filter(review_row::Column::ProviderReceiptId.is_null())
        .exec(txn)
        .await?;
    if changed.rows_affected != 1 {
        return Err(invalid());
    }
    Ok(())
}

pub(crate) struct ReviewCallUsage {
    pub dispatched: bool,
    pub tokens: Option<u64>,
    pub cost_micros: Option<u64>,
}

impl ReviewCallUsage {
    pub(crate) fn settlement(
        &self,
        reserved_tokens: i64,
        reserved_cost: i64,
    ) -> Result<desk_diagnose_core::approval_cost::ReviewUsageSettlement, DbErr> {
        desk_diagnose_core::approval_cost::ReviewUsageSettlement::from_provider_fact(
            self.dispatched,
            self.tokens,
            self.cost_micros,
            u64::try_from(reserved_tokens).map_err(|_| invalid())?,
            u64::try_from(reserved_cost).map_err(|_| invalid())?,
        )
        .ok_or_else(invalid)
    }
}

/// Settlement uses provider facts even after source/lease changes. It grants no
/// review verdict, tool permission, planner lease or repeat provider call.
pub(crate) async fn settle_review_call_on(
    txn: &DatabaseTransaction,
    row: &review_row::Model,
    claimed_tokens: Option<u64>,
    claimed_cost: Option<u64>,
    now_ms: i64,
) -> Result<ReviewCallUsage, DbErr> {
    let authority: ReviewCallAuthority =
        serde_json::from_str(row.call_authority_json.as_deref().ok_or_else(invalid)?)
            .map_err(|_| invalid())?;
    authority.validate().map_err(|_| invalid())?;
    let prices: desk_diagnose_core::approval_cost::ApprovalTokenPrices =
        serde_json::from_str(row.token_prices_json.as_deref().ok_or_else(invalid)?)
            .map_err(|_| invalid())?;
    prices.validate().ok_or_else(invalid)?;
    let dispatched = row.provider_receipt_id.is_some();
    if dispatched != row.provider_receipt_kind.is_some()
        || dispatched != row.provider_started_at_ms.is_some()
        || row.candidate_id != authority.candidate_id
        || row.conversation_id != authority.conversation_id
        || row.device_id != authority.device_id
        || row.actor_id != authority.actor_id
        || row.delegation_reservation_id.is_some() != authority.group_id.is_some()
    {
        return Err(invalid());
    }
    let actual = if let Some(id) = &row.provider_receipt_id {
        if row.provider_receipt_kind.as_deref() != Some("oss_model_egress") {
            return Err(invalid());
        }
        let call = crate::entity::model_egress_receipt::Entity::find_by_id(id)
            .one(txn)
            .await?;
        if let Some(call) = call {
            if let (Some(usage), Some(completed)) = (call.usage_json, call.usage_recorded_at) {
                let elapsed = u64::try_from(
                    completed
                        .timestamp_millis()
                        .checked_sub(call.authorized_at.timestamp_millis())
                        .ok_or_else(invalid)?,
                )
                .map_err(|_| invalid())?;
                desk_diagnose_core::subagent::reservation::known_model_usage(
                    serde_json::from_str(&usage).map_err(|_| invalid())?,
                    elapsed,
                )
            } else {
                None
            }
        } else {
            None
        }
    } else {
        Some(GoalUsage::default())
    };
    let token_usage = actual.map(|usage| desk_diagnose_core::chat::TokenUsage {
        input_tokens: i64::try_from(usage.input_tokens).ok(),
        output_tokens: i64::try_from(usage.output_tokens).ok(),
        cache_read_tokens: i64::try_from(usage.cache_read_tokens).ok(),
        cache_write_tokens: i64::try_from(usage.cache_write_tokens).ok(),
    });
    let tokens = token_usage.and_then(desk_diagnose_core::approval_review::reviewer_billed_tokens);
    let cost = token_usage.and_then(|usage| prices.actual(usage));
    if claimed_tokens
        .zip(tokens)
        .is_some_and(|(claimed, recorded)| claimed != recorded)
        || claimed_cost
            .zip(cost)
            .is_some_and(|(claimed, recorded)| claimed != recorded)
    {
        return Err(invalid());
    }
    if let Some(id) = &row.delegation_reservation_id {
        let cost_row = crate::entity::agent_delegation_reservation::Entity::find()
            .filter(crate::entity::agent_delegation_reservation::Column::ReservationId.eq(id))
            .one(txn)
            .await?
            .ok_or_else(invalid)?;
        let reservation = budget::decode_reservation(&cost_row)?;
        if reservation.review_authority.as_ref() != Some(&authority)
            || cost_row.provider_receipt_id != row.provider_receipt_id
            || cost_row.provider_receipt_kind != row.provider_receipt_kind
            || cost_row.provider_started_at_ms != row.provider_started_at_ms
        {
            return Err(invalid());
        }
        let allocation = actual
            .map(|actual| -> Result<Usage, DbErr> {
                Ok(Usage {
                    model_calls: u64::from(actual.model_calls),
                    tool_calls: u64::from(actual.tool_calls),
                    tokens: actual.total_tokens().ok_or_else(invalid)?,
                })
            })
            .transpose()?;
        funding::settle_call_budget_on(
            txn,
            &reservation,
            allocation,
            reservation.source_goal_upper.and(actual),
            now_ms,
        )
        .await?;
    }
    Ok(ReviewCallUsage {
        dispatched,
        tokens,
        cost_micros: cost,
    })
}

async fn validate_review_subject_on(
    txn: &DatabaseTransaction,
    authority: &ReviewCallAuthority,
    session_row: &session_row::Model,
    session: &PersistedAgentSession,
    now_ms: i64,
) -> Result<(), DbErr> {
    use desk_diagnose_core::approval_review::ApprovalSource;
    match &authority.source {
        ApprovalSource::PermissionItem {
            request_id,
            item_id,
        } => {
            let pending = session.permission_requests.iter().any(|request| {
                request.request_id == *request_id
                    && request.input_revision == authority.input_revision
                    && request.state
                        == desk_diagnose_core::dynamic_run::PermissionRequestState::Pending
                    && request.items.iter().any(|item| item.item_id == *item_id)
            });
            if !pending {
                return Err(invalid());
            }
        }
        ApprovalSource::ConcreteCall { call_id, grant_id } => {
            if !session.turn_state.is_active()
                || Some(session.lease_token) != authority.source_planning_lease_token
                || u64::try_from(session_row.lease_token).ok()
                    != authority.source_planning_lease_token
                || session_row
                    .lease_deadline
                    .is_none_or(|deadline| deadline.timestamp_millis() <= now_ms)
            {
                return Err(invalid());
            }
            let work = crate::entity::agent_action_item::Entity::find()
                .filter(crate::entity::agent_action_item::Column::ActionRequestId.eq(call_id))
                .filter(
                    crate::entity::agent_action_item::Column::ConversationId
                        .eq(&authority.conversation_id),
                )
                .filter(crate::entity::agent_action_item::Column::ActorId.eq(&authority.actor_id))
                .one(txn)
                .await?
                .ok_or_else(invalid)?;
            if work.status != crate::capability_grant_store::CAPABILITY_WORK_PREPARED
                || work.target_device_id != authority.device_id
                || session.current_turn_id.as_deref() != Some(work.turn_id.as_str())
            {
                return Err(invalid());
            }
            let stored = crate::entity::agent_capability_grant::Entity::find()
                .filter(crate::entity::agent_capability_grant::Column::GrantId.eq(grant_id))
                .filter(
                    crate::entity::agent_capability_grant::Column::RunId
                        .eq(&authority.conversation_id),
                )
                .filter(
                    crate::entity::agent_capability_grant::Column::ActorId.eq(&authority.actor_id),
                )
                .one(txn)
                .await?
                .ok_or_else(invalid)?;
            if stored.status != crate::capability_grant_store::GRANT_STATUS_ACTIVE {
                return Err(invalid());
            }
            let grant = crate::capability_grant_store::decode_grant(&stored)?;
            if grant.actor_id != authority.actor_id
                || grant.run_id != authority.conversation_id
                || grant.target_device_id != authority.device_id
                || grant.revoked_at_unix_ms.is_some()
                || grant.expires_at_unix_ms <= now_ms as u64
            {
                return Err(invalid());
            }
            crate::agent_approval_store::current_grant_parent_on(
                txn,
                session,
                &grant,
                now_ms as u64,
            )
            .await?;
        }

        _ => return Err(invalid()),
    }
    Ok(())
}
