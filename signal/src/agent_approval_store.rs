//! OSS owner delegation projection. All writes are subject-scoped SQLite
//! transactions; a saved switch is not a grant and cannot dispatch a tool.

use crate::entity::{
    agent_approval_delegation as delegation_row, agent_approval_review as review_row,
    agent_goal_run as goal_row, agent_run_event, agent_session,
};
use chrono::{DateTime, Utc};
use desk_diagnose_core::approval_delegation::{
    ApprovalDelegation, ApprovalDelegationAuditEvent, ApprovalDelegationStatus,
};
use desk_diagnose_core::approval_review::{
    APPROVAL_REVIEW_SOURCE_PERMISSION_ITEM, APPROVAL_REVIEW_STATUS_APPROVED, ApprovalAuthorityFact,
    MAX_APPROVAL_REFERENCES, permission_review_candidate_id, permission_review_source_id,
    project_permission_authority,
};
use desk_diagnose_core::dynamic_run::{
    AGENT_RUN_EVENT_SCHEMA_VERSION, AgentRunEvent, AgentRunEventKind, PermissionDecidedEvent,
    PermissionDecisionSource, PermissionItemDecision,
};
use desk_diagnose_core::session::{AgentSessionSurface, PersistedAgentSession};
use sea_orm::sea_query::Expr;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, DatabaseTransaction, DbErr, EntityTrait,
    QueryFilter, QueryOrder, QuerySelect, Set, TransactionTrait,
};

/// A claimed review is the sole permission to make one external reviewer call.
/// The raw candidate is never stored a second time in SQLite.
pub struct ClaimedPermissionReview {
    pub candidate_id: String,
    pub lease_epoch: u64,
    pub lease_owner: String,
    pub lease_deadline_unix_ms: u64,
    pub model_destination: desk_agent_protocol::data_lineage::DestinationIdentity,
    pub model_config_revision: u64,
    pub prices: desk_diagnose_core::approval_cost::ApprovalTokenPrices,
    pub authorized: desk_diagnose_core::approval_egress::AuthorizedApprovalReview,
}

pub struct PendingPermissionReview {
    pub conversation_id: String,
    pub owner_id: String,
    pub device_id: String,
    pub request_id: String,
}

/// Page active delegations rather than scanning unbounded conversation JSON.
/// This only discovers work; preparation and claim repeat all authority checks.
pub async fn pending_permission_reviews(
    db: &DatabaseConnection,
    after_id: i64,
    limit: u64,
    _now_unix_ms: u64,
) -> Result<(Vec<PendingPermissionReview>, Option<i64>), DbErr> {
    use desk_diagnose_core::dynamic_run::PermissionRequestState;
    let page_size = limit.clamp(1, 64);
    let rows = delegation_row::Entity::find()
        .filter(delegation_row::Column::Id.gt(after_id))
        .filter(delegation_row::Column::Status.eq("active"))
        .order_by_asc(delegation_row::Column::Id)
        .limit(page_size)
        .all(db)
        .await?;
    let next = (rows.len() == page_size as usize)
        .then(|| rows.last().map(|row| row.id))
        .flatten();
    let mut pending = Vec::new();
    for row in rows {
        let session_row = agent_session::Entity::find()
            .filter(agent_session::Column::ConversationId.eq(&row.conversation_id))
            .filter(agent_session::Column::ActorId.eq(&row.actor_id))
            .filter(agent_session::Column::DeviceId.eq(&row.device_id))
            .one(db)
            .await?;
        let Some(session_row) = session_row else {
            continue;
        };
        let session =
            PersistedAgentSession::decode_json(&session_row.state_json).map_err(|_| invalid())?;
        if session.surface != AgentSessionSurface::AiAssistant
            || session.turn_state.is_active()
            || !session.trigger_origin.allows_delegated_review()
        {
            continue;
        }
        for request in &session.permission_requests {
            if request.state == PermissionRequestState::Pending
                && request.input_revision == session.input_revision
            {
                pending.push(PendingPermissionReview {
                    conversation_id: row.conversation_id.clone(),
                    owner_id: row.actor_id.clone(),
                    device_id: row.device_id.clone(),
                    request_id: request.request_id.clone(),
                });
            }
        }
    }
    Ok((pending, next))
}

/// Rebuild the reviewer-facing authorization snapshot from durable decisions
/// and grants, never from a historical tool-result string. The target request
/// is mandatory; recent other requests are included only while the bounded
/// reviewer context has room for complete per-item facts.
pub(crate) async fn permission_authority_on(
    txn: &DatabaseTransaction,
    session: &PersistedAgentSession,
    target_request_id: &str,
    readiness_revision: u64,
    now_unix_ms: u64,
) -> Result<Vec<ApprovalAuthorityFact>, DbErr> {
    let grants = crate::capability_grant_store::SignalCapabilityGrantStore::list_for_subject_on(
        txn,
        &session.conversation_id,
        &session.actor_id,
        &session.device_id,
    )
    .await?;
    let target = session
        .permission_requests
        .iter()
        .find(|request| request.request_id == target_request_id)
        .ok_or_else(invalid)?;
    let mut facts = permission_request_authority_on(
        txn,
        session,
        target,
        &grants,
        readiness_revision,
        now_unix_ms,
    )
    .await?;
    if facts.len() > MAX_APPROVAL_REFERENCES {
        return Err(invalid());
    }
    for request in session.permission_requests.iter().rev() {
        if request.request_id == target_request_id {
            continue;
        }
        if facts.len().saturating_add(request.items.len()) > MAX_APPROVAL_REFERENCES {
            continue;
        }
        facts.extend(
            permission_request_authority_on(
                txn,
                session,
                request,
                &grants,
                readiness_revision,
                now_unix_ms,
            )
            .await?,
        );
    }
    Ok(facts)
}

async fn permission_request_authority_on(
    txn: &DatabaseTransaction,
    session: &PersistedAgentSession,
    request: &desk_diagnose_core::dynamic_run::PermissionRequest,
    grants: &[desk_agent_protocol::capability_grant::CapabilityGrant],
    readiness_revision: u64,
    now_unix_ms: u64,
) -> Result<Vec<ApprovalAuthorityFact>, DbErr> {
    let rows = agent_run_event::Entity::find()
        .filter(agent_run_event::Column::RunId.eq(&session.conversation_id))
        .filter(agent_run_event::Column::Kind.eq(AgentRunEventKind::PermissionDecided.as_str()))
        .filter(agent_run_event::Column::CorrelationId.eq(&request.request_id))
        .all(txn)
        .await?;
    if rows.len() > 1 {
        return Err(invalid());
    }
    let decision = rows
        .first()
        .map(|row| {
            serde_json::from_str::<PermissionDecidedEvent>(&row.payload_json).map_err(|_| invalid())
        })
        .transpose()?;
    project_permission_authority(
        &session.conversation_id,
        &session.actor_id,
        &session.device_id,
        request,
        decision.as_ref(),
        grants,
        now_unix_ms,
        readiness_revision,
    )
    .map_err(|_| invalid())
}

/// Claim and reserve one exact item in a single local write transaction. A
/// duplicate candidate is never redialed: if an earlier dial outcome is unknown,
/// recovery charges the reservation and hands the item to the owner.
#[allow(clippy::too_many_arguments)]
pub async fn claim_permission_review(
    db: &DatabaseConnection,
    candidate: &desk_diagnose_core::approval_review::ApprovalReviewCandidate,
    registry: &desk_diagnose_core::provider_registry::ProviderRegistry,
    surface: desk_agent_protocol::capability_provider::ProductSurface,
    lease_owner: &str,
    readiness_revision: u64,
    now_unix_ms: u64,
) -> Result<Option<ClaimedPermissionReview>, DbErr> {
    use desk_diagnose_core::approval_egress::authorize_permission_review_egress;
    use desk_diagnose_core::approval_review::{
        APPROVAL_REVIEW_STATUS_REVIEWING, candidate_context_hmac_sha256,
    };
    candidate.validate().map_err(|_| invalid())?;
    if lease_owner.trim().is_empty()
        || lease_owner.len() > 256
        || lease_owner.chars().any(char::is_control)
        || readiness_revision == 0
        || now_unix_ms >= candidate.expires_at_unix_ms
    {
        return Err(invalid());
    }
    let key = crate::approval_review_secret::load_or_create(db).await?;
    let context_hmac = candidate_context_hmac_sha256(&key, candidate).map_err(|_| invalid())?;
    let txn = crate::db::begin_write(db, delegation_row::Entity).await?;
    let (_, session) = session_on(
        &txn,
        &candidate.conversation_id,
        &candidate.owner_id,
        &candidate.device_id,
    )
    .await?;
    let config = crate::approval_model_provider::load(&txn).await?;
    let destination = config.destination_identity().map_err(|_| invalid())?;
    let prices = config.prices.ok_or_else(invalid)?;
    let delegation_row = delegation_row::Entity::find()
        .filter(delegation_row::Column::DelegationId.eq(&candidate.delegation_id))
        .filter(delegation_row::Column::ConversationId.eq(&candidate.conversation_id))
        .filter(delegation_row::Column::ActorId.eq(&candidate.owner_id))
        .filter(delegation_row::Column::DeviceId.eq(&candidate.device_id))
        .one(&txn)
        .await?
        .ok_or_else(invalid)?;
    let mut delegation = decode(&delegation_row)?;
    let goal_row = goal_row::Entity::find()
        .filter(goal_row::Column::ConversationId.eq(&candidate.conversation_id))
        .filter(goal_row::Column::ActorId.eq(&candidate.owner_id))
        .filter(goal_row::Column::DeviceId.eq(&candidate.device_id))
        .filter(goal_row::Column::Status.is_not_in(["completed", "failed", "cancelled"]))
        .one(&txn)
        .await?;
    let goal = goal_row
        .as_ref()
        .map(crate::agent_goal_store::decode)
        .transpose()?;
    let request = session
        .permission_requests
        .iter()
        .find(|request| {
            matches!(&candidate.source,
            desk_diagnose_core::approval_review::ApprovalSource::PermissionItem { request_id, .. }
                if request_id == &request.request_id)
        })
        .ok_or_else(invalid)?;
    let item_id = match &candidate.source {
        desk_diagnose_core::approval_review::ApprovalSource::PermissionItem { item_id, .. } => {
            item_id
        }
        _ => return Err(invalid()),
    };
    let item = request
        .items
        .iter()
        .find(|item| &item.item_id == item_id)
        .ok_or_else(invalid)?;
    let (descriptor, risk) =
        desk_diagnose_core::approval_review::trusted_permission_descriptor(registry, item, surface)
            .map_err(|_| invalid())?;
    if candidate.descriptor_json != descriptor || candidate.risk != risk {
        return Err(invalid());
    }
    if candidate.context.current_authority
        != permission_authority_on(
            &txn,
            &session,
            &request.request_id,
            readiness_revision,
            now_unix_ms,
        )
        .await?
    {
        return Err(invalid());
    }
    let authorized = authorize_permission_review_egress(
        candidate,
        &session,
        request,
        goal.as_ref(),
        &delegation,
        &destination,
        now_unix_ms,
    )
    .map_err(|_| invalid())?;
    if let Some(existing) = review_row::Entity::find()
        .filter(review_row::Column::CandidateId.eq(&candidate.candidate_id))
        .one(&txn)
        .await?
    {
        if existing.conversation_id != candidate.conversation_id
            || existing.actor_id != candidate.owner_id
            || existing.device_id != candidate.device_id
            || existing.delegation_id != candidate.delegation_id
            || existing.action_sha256 != candidate.action_sha256
            || existing.context_hmac_sha256 != context_hmac
        {
            return Err(invalid());
        }
        txn.commit().await?;
        return Ok(None);
    }
    let (reserved_tokens, reserved_cost_micros) =
        desk_diagnose_core::approval_cost::reviewer_reservation(
            &authorized.prompt,
            prices,
            u64::try_from(config.gateway.max_context_bytes.ok_or_else(invalid)?)
                .map_err(|_| invalid())?,
        )
        .ok_or_else(invalid)?;
    delegation
        .reserve(reserved_tokens, reserved_cost_micros)
        .map_err(|_| invalid())?;
    let changed = delegation_row::Entity::update_many()
        .col_expr(
            delegation_row::Column::StateJson,
            Expr::value(serde_json::to_string(&delegation).map_err(|_| invalid())?),
        )
        .col_expr(
            delegation_row::Column::Version,
            Expr::value(as_i64(delegation.ledger_version)?),
        )
        .col_expr(
            delegation_row::Column::UpdatedAt,
            Expr::value(as_i64(now_unix_ms)?),
        )
        .filter(delegation_row::Column::Id.eq(delegation_row.id))
        .filter(delegation_row::Column::Version.eq(delegation_row.version))
        .filter(delegation_row::Column::Status.eq("active"))
        .exec(&txn)
        .await?;
    if changed.rows_affected != 1 {
        return Err(invalid());
    }
    let lease_deadline_unix_ms = now_unix_ms
        .saturating_add(desk_diagnose_core::approval_review::APPROVAL_REVIEW_LEASE_MS)
        .min(candidate.expires_at_unix_ms);
    let (request_id, item_id) = match &candidate.source {
        desk_diagnose_core::approval_review::ApprovalSource::PermissionItem {
            request_id,
            item_id,
        } => (request_id, item_id),
        _ => return Err(invalid()),
    };
    review_row::ActiveModel {
        candidate_id: Set(candidate.candidate_id.clone()),
        conversation_id: Set(candidate.conversation_id.clone()),
        actor_id: Set(candidate.owner_id.clone()),
        device_id: Set(candidate.device_id.clone()),
        delegation_id: Set(candidate.delegation_id.clone()),
        model_config_revision: Set(as_i64(
            u64::try_from(config.configuration_revision).map_err(|_| invalid())?,
        )?),
        source_kind: Set(APPROVAL_REVIEW_SOURCE_PERMISSION_ITEM.into()),
        source_id: Set(permission_review_source_id(request_id, item_id).map_err(|_| invalid())?),
        action_sha256: Set(candidate.action_sha256.clone()),
        context_hmac_sha256: Set(context_hmac),
        decision_json: Set(None),
        status: Set(APPROVAL_REVIEW_STATUS_REVIEWING.into()),
        lease_epoch: Set(1),
        lease_owner: Set(Some(lease_owner.to_owned())),
        lease_deadline: Set(Some(as_i64(lease_deadline_unix_ms)?)),
        reserved_tokens: Set(as_i64(reserved_tokens)?),
        reserved_cost_micros: Set(as_i64(reserved_cost_micros)?),
        expires_at: Set(as_i64(candidate.expires_at_unix_ms)?),
        created_at: Set(as_i64(now_unix_ms)?),
        updated_at: Set(as_i64(now_unix_ms)?),
        ..Default::default()
    }
    .insert(&txn)
    .await?;
    txn.commit().await?;
    Ok(Some(ClaimedPermissionReview {
        candidate_id: candidate.candidate_id.clone(),
        lease_epoch: 1,
        lease_owner: lease_owner.to_owned(),
        lease_deadline_unix_ms,
        model_destination: destination,
        model_config_revision: u64::try_from(config.configuration_revision)
            .map_err(|_| invalid())?,
        prices,
        authorized,
    }))
}

/// Charge one claimed review and record its bounded verdict. An approve row is
/// not a grant: the entire permission request is committed only after every
/// item has a current final review in the permission-decision transaction.
#[allow(clippy::too_many_arguments)]
pub async fn settle_permission_review(
    db: &DatabaseConnection,
    candidate: &desk_diagnose_core::approval_review::ApprovalReviewCandidate,
    lease_owner: &str,
    lease_epoch: u64,
    decision: Option<&desk_diagnose_core::approval_review::ApprovalReviewDecision>,
    actual_tokens: Option<u64>,
    actual_cost_micros: Option<u64>,
    readiness_revision: u64,
    now_unix_ms: u64,
) -> Result<String, DbErr> {
    use desk_diagnose_core::approval_egress::authorize_permission_review_egress;
    use desk_diagnose_core::approval_review::{
        APPROVAL_REVIEW_STATUS_DENIED, APPROVAL_REVIEW_STATUS_EXPIRED,
        APPROVAL_REVIEW_STATUS_REVIEWING, APPROVAL_REVIEW_STATUS_UNAVAILABLE, ApprovalSource,
        ApprovalVerdict, candidate_context_hmac_sha256,
    };
    candidate.validate().map_err(|_| invalid())?;
    let key = crate::approval_review_secret::load_or_create(db).await?;
    let context_hmac = candidate_context_hmac_sha256(&key, candidate).map_err(|_| invalid())?;
    let txn = crate::db::begin_write(db, review_row::Entity).await?;
    let row = review_row::Entity::find()
        .filter(review_row::Column::CandidateId.eq(&candidate.candidate_id))
        .one(&txn)
        .await?
        .ok_or_else(invalid)?;
    if row.status != APPROVAL_REVIEW_STATUS_REVIEWING
        || row.lease_owner.as_deref() != Some(lease_owner)
        || row.lease_epoch != as_i64(lease_epoch)?
        || row.context_hmac_sha256 != context_hmac
        || row.action_sha256 != candidate.action_sha256
        || row.expires_at != as_i64(candidate.expires_at_unix_ms)?
        || row.reserved_tokens <= 0
        || row.reserved_cost_micros <= 0
    {
        return Err(invalid());
    }
    let (_, session) = session_on(
        &txn,
        &candidate.conversation_id,
        &candidate.owner_id,
        &candidate.device_id,
    )
    .await?;
    let stored = delegation_row::Entity::find()
        .filter(delegation_row::Column::DelegationId.eq(&candidate.delegation_id))
        .one(&txn)
        .await?
        .ok_or_else(invalid)?;
    let mut delegation = decode(&stored)?;
    let goal_row = goal_row::Entity::find()
        .filter(goal_row::Column::ConversationId.eq(&candidate.conversation_id))
        .filter(goal_row::Column::ActorId.eq(&candidate.owner_id))
        .filter(goal_row::Column::DeviceId.eq(&candidate.device_id))
        .filter(goal_row::Column::Status.is_not_in(["completed", "failed", "cancelled"]))
        .one(&txn)
        .await?;
    let goal = goal_row
        .as_ref()
        .map(crate::agent_goal_store::decode)
        .transpose()?;
    let request = match &candidate.source {
        ApprovalSource::PermissionItem { request_id, .. } => session
            .permission_requests
            .iter()
            .find(|request| &request.request_id == request_id),
        _ => None,
    };
    let config = crate::approval_model_provider::load(&txn).await?;
    let model_revision_matches =
        i64::try_from(config.configuration_revision).ok() == Some(row.model_config_revision);
    let authority_matches = match request {
        Some(request) if readiness_revision != 0 => permission_authority_on(
            &txn,
            &session,
            &request.request_id,
            readiness_revision,
            now_unix_ms,
        )
        .await
        .is_ok_and(|facts| facts == candidate.context.current_authority),
        _ => false,
    };
    let authorized = match (request, config.destination_identity()) {
        (Some(request), Ok(destination)) if model_revision_matches => {
            authority_matches
                && authorize_permission_review_egress(
                    candidate,
                    &session,
                    request,
                    goal.as_ref(),
                    &delegation,
                    &destination,
                    now_unix_ms,
                )
                .is_ok()
        }
        _ => false,
    };
    let now_i64 = as_i64(now_unix_ms)?;
    let expired = now_unix_ms >= candidate.expires_at_unix_ms
        || row
            .lease_deadline
            .is_some_and(|deadline| deadline <= now_i64);
    let usage_in_bounds = actual_tokens.is_none_or(|used| used <= row.reserved_tokens as u64)
        && actual_cost_micros.is_none_or(|used| used <= row.reserved_cost_micros as u64);
    let valid_decision = decision.is_some_and(|review| review.validate_for(candidate).is_ok());
    let status = if expired {
        APPROVAL_REVIEW_STATUS_EXPIRED
    } else if !authorized || !usage_in_bounds || !valid_decision {
        APPROVAL_REVIEW_STATUS_UNAVAILABLE
    } else {
        match decision.map(|review| review.verdict) {
            Some(ApprovalVerdict::Approve) => APPROVAL_REVIEW_STATUS_APPROVED,
            Some(ApprovalVerdict::Deny) => APPROVAL_REVIEW_STATUS_DENIED,
            _ => APPROVAL_REVIEW_STATUS_UNAVAILABLE,
        }
    };
    delegation
        .settle(
            row.reserved_tokens as u64,
            row.reserved_cost_micros as u64,
            usage_in_bounds.then_some(actual_tokens).flatten(),
            usage_in_bounds.then_some(actual_cost_micros).flatten(),
        )
        .map_err(|_| invalid())?;
    let changed = delegation_row::Entity::update_many()
        .col_expr(
            delegation_row::Column::StateJson,
            Expr::value(serde_json::to_string(&delegation).map_err(|_| invalid())?),
        )
        .col_expr(
            delegation_row::Column::Version,
            Expr::value(as_i64(delegation.ledger_version)?),
        )
        .col_expr(
            delegation_row::Column::UpdatedAt,
            Expr::value(as_i64(now_unix_ms)?),
        )
        .filter(delegation_row::Column::Id.eq(stored.id))
        .filter(delegation_row::Column::Version.eq(stored.version))
        .exec(&txn)
        .await?;
    if changed.rows_affected != 1 {
        return Err(invalid());
    }
    let recorded_decision = if valid_decision && usage_in_bounds {
        decision
            .map(|review| serde_json::to_string(review))
            .transpose()
            .map_err(|_| invalid())?
    } else {
        None
    };
    let changed = review_row::Entity::update_many()
        .col_expr(review_row::Column::Status, Expr::value(status))
        .col_expr(
            review_row::Column::DecisionJson,
            Expr::value(recorded_decision),
        )
        .col_expr(
            review_row::Column::LeaseOwner,
            Expr::value(Option::<String>::None),
        )
        .col_expr(
            review_row::Column::LeaseDeadline,
            Expr::value(Option::<i64>::None),
        )
        .col_expr(
            review_row::Column::UpdatedAt,
            Expr::value(as_i64(now_unix_ms)?),
        )
        .filter(review_row::Column::Id.eq(row.id))
        .filter(review_row::Column::Status.eq(APPROVAL_REVIEW_STATUS_REVIEWING))
        .filter(review_row::Column::LeaseEpoch.eq(row.lease_epoch))
        .filter(review_row::Column::LeaseOwner.eq(lease_owner))
        .exec(&txn)
        .await?;
    if changed.rows_affected != 1 {
        return Err(invalid());
    }
    txn.commit().await?;
    Ok(status.to_owned())
}

/// A crashed or timed-out reviewer cannot be called again with the same
/// candidate. Charge the full reservation so the pending request can receive
/// a system-sourced, unexecuted denial; a late result loses the lease fence.
pub async fn expire_review_leases(
    db: &DatabaseConnection,
    now_unix_ms: u64,
    limit: u64,
) -> Result<u64, DbErr> {
    use desk_diagnose_core::approval_review::{
        APPROVAL_REVIEW_STATUS_EXPIRED, APPROVAL_REVIEW_STATUS_REVIEWING,
        APPROVAL_REVIEW_STATUS_UNAVAILABLE,
    };
    let now = as_i64(now_unix_ms)?;
    let rows = review_row::Entity::find()
        .filter(review_row::Column::Status.eq(APPROVAL_REVIEW_STATUS_REVIEWING))
        .filter(review_row::Column::LeaseDeadline.lte(now))
        .order_by_asc(review_row::Column::LeaseDeadline)
        .order_by_asc(review_row::Column::Id)
        .limit(limit.min(64))
        .all(db)
        .await?;
    let mut settled = 0;
    for snapshot in rows {
        let txn = crate::db::begin_write(db, review_row::Entity).await?;
        let Some(row) = review_row::Entity::find_by_id(snapshot.id)
            .one(&txn)
            .await?
        else {
            continue;
        };
        if row.status != APPROVAL_REVIEW_STATUS_REVIEWING
            || row.lease_deadline.is_none_or(|deadline| deadline > now)
        {
            continue;
        }
        let stored = delegation_row::Entity::find()
            .filter(delegation_row::Column::DelegationId.eq(&row.delegation_id))
            .filter(delegation_row::Column::ConversationId.eq(&row.conversation_id))
            .filter(delegation_row::Column::ActorId.eq(&row.actor_id))
            .filter(delegation_row::Column::DeviceId.eq(&row.device_id))
            .one(&txn)
            .await?
            .ok_or_else(invalid)?;
        let mut delegation = decode(&stored)?;
        if row.reserved_tokens <= 0 || row.reserved_cost_micros <= 0 {
            return Err(invalid());
        }
        delegation
            .settle(
                row.reserved_tokens as u64,
                row.reserved_cost_micros as u64,
                None,
                None,
            )
            .map_err(|_| invalid())?;
        let changed = delegation_row::Entity::update_many()
            .col_expr(
                delegation_row::Column::StateJson,
                Expr::value(serde_json::to_string(&delegation).map_err(|_| invalid())?),
            )
            .col_expr(
                delegation_row::Column::Version,
                Expr::value(as_i64(delegation.ledger_version)?),
            )
            .col_expr(delegation_row::Column::UpdatedAt, Expr::value(now))
            .filter(delegation_row::Column::Id.eq(stored.id))
            .filter(delegation_row::Column::Version.eq(stored.version))
            .exec(&txn)
            .await?;
        if changed.rows_affected != 1 {
            return Err(invalid());
        }
        let status = if row.expires_at <= now {
            APPROVAL_REVIEW_STATUS_EXPIRED
        } else {
            APPROVAL_REVIEW_STATUS_UNAVAILABLE
        };
        let changed = review_row::Entity::update_many()
            .col_expr(review_row::Column::Status, Expr::value(status))
            .col_expr(
                review_row::Column::LeaseOwner,
                Expr::value(Option::<String>::None),
            )
            .col_expr(
                review_row::Column::LeaseDeadline,
                Expr::value(Option::<i64>::None),
            )
            .col_expr(review_row::Column::UpdatedAt, Expr::value(now))
            .filter(review_row::Column::Id.eq(row.id))
            .filter(review_row::Column::Status.eq(APPROVAL_REVIEW_STATUS_REVIEWING))
            .filter(review_row::Column::LeaseEpoch.eq(row.lease_epoch))
            .exec(&txn)
            .await?;
        if changed.rows_affected != 1 {
            return Err(invalid());
        }
        txn.commit().await?;
        settled += 1;
    }
    Ok(settled)
}

pub(crate) struct ReviewedPermissionBatch {
    pub decisions: Vec<desk_diagnose_core::dynamic_run::PermissionDecisionItem>,
    pub source: PermissionDecisionSource,
    pub delegation: ApprovalDelegation,
    pub model_config_revision: u64,
    pub goal_binding: Option<(String, u64)>,
}

/// A competing coordinator must not turn an in-flight review into a fault
/// denial. The caller holds the session write transaction while checking this
/// immediately before recording ReviewUnavailable.
pub(crate) async fn has_live_permission_review_on(
    txn: &DatabaseTransaction,
    session: &PersistedAgentSession,
    request: &desk_diagnose_core::dynamic_run::PermissionRequest,
    now_unix_ms: u64,
) -> Result<bool, DbErr> {
    let source_ids = request
        .items
        .iter()
        .map(|item| permission_review_source_id(&request.request_id, &item.item_id))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| invalid())?;
    if source_ids.is_empty() {
        return Ok(false);
    }
    Ok(review_row::Entity::find()
        .filter(review_row::Column::ConversationId.eq(&session.conversation_id))
        .filter(review_row::Column::ActorId.eq(&session.actor_id))
        .filter(review_row::Column::DeviceId.eq(&session.device_id))
        .filter(review_row::Column::SourceKind.eq(APPROVAL_REVIEW_SOURCE_PERMISSION_ITEM))
        .filter(review_row::Column::SourceId.is_in(source_ids))
        .filter(
            review_row::Column::Status
                .eq(desk_diagnose_core::approval_review::APPROVAL_REVIEW_STATUS_REVIEWING),
        )
        .filter(review_row::Column::LeaseDeadline.gt(as_i64(now_unix_ms)?))
        .one(txn)
        .await?
        .is_some())
}

/// Reconstruct every review from current rows inside the final permission
/// transaction. Historical model text and returned candidate objects alone are
/// never enough to issue grants.
pub(crate) async fn reviewed_permission_batch_on(
    txn: &DatabaseTransaction,
    session: &PersistedAgentSession,
    request: &desk_diagnose_core::dynamic_run::PermissionRequest,
    candidates: &[desk_diagnose_core::approval_review::ApprovalReviewCandidate],
    registry: &desk_diagnose_core::provider_registry::ProviderRegistry,
    surface: desk_agent_protocol::capability_provider::ProductSurface,
    hmac_key: &[u8; 32],
    readiness_revision: u64,
    now_unix_ms: u64,
) -> Result<ReviewedPermissionBatch, DbErr> {
    use desk_diagnose_core::approval_egress::authorize_permission_review_egress;
    use desk_diagnose_core::approval_review::{
        ApprovalReviewDecision, ApprovalVerdict, candidate_context_hmac_sha256,
        permission_decision_from_reviews,
    };
    if candidates.len() != request.items.len() || candidates.is_empty() {
        return Err(invalid());
    }
    let first = &candidates[0];
    let stored = delegation_row::Entity::find()
        .filter(delegation_row::Column::DelegationId.eq(&first.delegation_id))
        .filter(delegation_row::Column::ConversationId.eq(&session.conversation_id))
        .filter(delegation_row::Column::ActorId.eq(&session.actor_id))
        .filter(delegation_row::Column::DeviceId.eq(&session.device_id))
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    let delegation = decode(&stored)?;
    let config = crate::approval_model_provider::load(txn).await?;
    let destination = config.destination_identity().map_err(|_| invalid())?;
    let model_config_revision =
        u64::try_from(config.configuration_revision).map_err(|_| invalid())?;
    let goal_row = goal_row::Entity::find()
        .filter(goal_row::Column::ConversationId.eq(&session.conversation_id))
        .filter(goal_row::Column::ActorId.eq(&session.actor_id))
        .filter(goal_row::Column::DeviceId.eq(&session.device_id))
        .filter(goal_row::Column::Status.is_not_in(["completed", "failed", "cancelled"]))
        .one(txn)
        .await?;
    let goal = goal_row
        .as_ref()
        .map(crate::agent_goal_store::decode)
        .transpose()?;
    let current_authority = permission_authority_on(
        txn,
        session,
        &request.request_id,
        readiness_revision,
        now_unix_ms,
    )
    .await?;
    let mut reviews = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        let item_id = match &candidate.source {
            desk_diagnose_core::approval_review::ApprovalSource::PermissionItem {
                item_id, ..
            } => item_id,
            _ => return Err(invalid()),
        };
        let item = request
            .items
            .iter()
            .find(|item| &item.item_id == item_id)
            .ok_or_else(invalid)?;
        let (descriptor, risk) =
            desk_diagnose_core::approval_review::trusted_permission_descriptor(
                registry, item, surface,
            )
            .map_err(|_| invalid())?;
        if candidate.descriptor_json != descriptor || candidate.risk != risk {
            return Err(invalid());
        }
        if candidate.context.current_authority != current_authority {
            return Err(invalid());
        }
        authorize_permission_review_egress(
            candidate,
            session,
            request,
            goal.as_ref(),
            &delegation,
            &destination,
            now_unix_ms,
        )
        .map_err(|_| invalid())?;
        let (request_id, item_id) = match &candidate.source {
            desk_diagnose_core::approval_review::ApprovalSource::PermissionItem {
                request_id,
                item_id,
            } => (request_id, item_id),
            _ => return Err(invalid()),
        };
        let row = review_row::Entity::find()
            .filter(review_row::Column::CandidateId.eq(&candidate.candidate_id))
            .filter(review_row::Column::ConversationId.eq(&session.conversation_id))
            .filter(review_row::Column::ActorId.eq(&session.actor_id))
            .filter(review_row::Column::DeviceId.eq(&session.device_id))
            .one(txn)
            .await?
            .ok_or_else(invalid)?;
        let expected_hmac =
            candidate_context_hmac_sha256(hmac_key, candidate).map_err(|_| invalid())?;
        if row.delegation_id != delegation.delegation_id
            || row.model_config_revision != as_i64(model_config_revision)?
            || row.source_kind != APPROVAL_REVIEW_SOURCE_PERMISSION_ITEM
            || row.source_id
                != permission_review_source_id(request_id, item_id).map_err(|_| invalid())?
            || row.action_sha256 != candidate.action_sha256
            || row.context_hmac_sha256 != expected_hmac
            || row.lease_owner.is_some()
            || row.lease_deadline.is_some()
            || row.expires_at != as_i64(candidate.expires_at_unix_ms)?
        {
            return Err(invalid());
        }
        let review: ApprovalReviewDecision =
            serde_json::from_str(row.decision_json.as_deref().ok_or_else(invalid)?)
                .map_err(|_| invalid())?;
        review.validate_for(candidate).map_err(|_| invalid())?;
        let status = match review.verdict {
            ApprovalVerdict::Approve => APPROVAL_REVIEW_STATUS_APPROVED,
            ApprovalVerdict::Deny => {
                desk_diagnose_core::approval_review::APPROVAL_REVIEW_STATUS_DENIED
            }
        };
        if row.status != status {
            return Err(invalid());
        }
        reviews.push(review);
    }
    let (decisions, source) =
        permission_decision_from_reviews(request, &delegation, candidates, &reviews)
            .map_err(|_| invalid())?;
    Ok(ReviewedPermissionBatch {
        decisions,
        source,
        delegation,
        model_config_revision,
        goal_binding: goal.map(|goal| (goal.goal_id, goal.goal_revision)),
    })
}

/// Reconstruct the AI grant parent from current rows inside the dispatch
/// transaction. Grant payloads and historical tool text are never authority.
pub(crate) async fn current_grant_parent_on(
    txn: &DatabaseTransaction,
    session: &PersistedAgentSession,
    grant: &desk_agent_protocol::capability_grant::CapabilityGrant,
    now_unix_ms: u64,
) -> Result<desk_agent_protocol::capability_grant::AiApprovalGrantProvenance, DbErr> {
    use desk_agent_protocol::capability_grant::{AiApprovalGrantProvenance, CapabilityGrantIssuer};
    use desk_diagnose_core::approval_review::{ApprovalReviewDecision, ApprovalVerdict};
    let CapabilityGrantIssuer::AiApproval(parent) = &grant.issued_by else {
        return Err(invalid());
    };
    if session.surface != AgentSessionSurface::AiAssistant
        || session.conversation_id != grant.run_id
        || session.actor_id != grant.actor_id
        || session.device_id != grant.target_device_id
    {
        return Err(invalid());
    }
    let row = delegation_row::Entity::find()
        .filter(delegation_row::Column::DelegationId.eq(&parent.delegation_id))
        .filter(delegation_row::Column::ConversationId.eq(&session.conversation_id))
        .filter(delegation_row::Column::ActorId.eq(&session.actor_id))
        .filter(delegation_row::Column::DeviceId.eq(&session.device_id))
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    let delegation = decode(&row)?;
    let config = crate::approval_model_provider::load(txn).await?;
    if config.unavailable_reason().is_some() {
        return Err(invalid());
    }
    let goal = current_goal_on(
        txn,
        &session.conversation_id,
        &session.actor_id,
        &session.device_id,
    )
    .await?;
    delegation
        .require_current(
            &session.conversation_id,
            &session.actor_id,
            &session.device_id,
        )
        .map_err(|_| invalid())?;
    let event_row = agent_run_event::Entity::find()
        .filter(agent_run_event::Column::EventId.eq(&parent.decision_event_id))
        .filter(agent_run_event::Column::RunId.eq(&session.conversation_id))
        .filter(agent_run_event::Column::Kind.eq(AgentRunEventKind::PermissionDecided.as_str()))
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    let event: PermissionDecidedEvent =
        serde_json::from_str(&event_row.payload_json).map_err(|_| invalid())?;
    event.validate().map_err(|_| invalid())?;
    let PermissionDecisionSource::AiApproval {
        delegation_id,
        delegation_revision,
        reviews,
    } = &event.decision_source
    else {
        return Err(invalid());
    };
    if delegation_id != &delegation.delegation_id
        || *delegation_revision != delegation.revision
        || event.event.event_id != event_row.event_id
        || event.event.run_id != session.conversation_id
        || event.request_input_revision != grant.input_revision
    {
        return Err(invalid());
    }
    let request = session
        .permission_requests
        .iter()
        .find(|request| request.request_id == event.request_id)
        .ok_or_else(invalid)?;
    if !matches!(
        request.state,
        desk_diagnose_core::dynamic_run::PermissionRequestState::Approved
            | desk_diagnose_core::dynamic_run::PermissionRequestState::PartiallyApproved
    ) || request.input_revision != grant.input_revision
        || request.state != event.resulting_state
    {
        return Err(invalid());
    }
    let item = request
        .items
        .iter()
        .find(|item| {
            desk_diagnose_core::permission_grant::permission_item_grant_id(
                &session.conversation_id,
                request,
                &item.item_id,
            ) == grant.grant_id
        })
        .ok_or_else(invalid)?;
    if item.provider_id != grant.provider_id
        || item.tool_name != grant.tool_name
        || item.expected_effect != grant.effect
        || !event.items.iter().any(|decision| {
            decision.item_id == item.item_id
                && matches!(decision.decision, PermissionItemDecision::Approve { .. })
        })
    {
        return Err(invalid());
    }
    let review = reviews
        .iter()
        .find(|review| review.item_id == item.item_id)
        .ok_or_else(invalid)?;
    let review_row = review_row::Entity::find()
        .filter(review_row::Column::CandidateId.eq(&review.candidate_id))
        .filter(review_row::Column::ConversationId.eq(&session.conversation_id))
        .filter(review_row::Column::ActorId.eq(&session.actor_id))
        .filter(review_row::Column::DeviceId.eq(&session.device_id))
        .filter(review_row::Column::DelegationId.eq(&delegation.delegation_id))
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    let expected_candidate = permission_review_candidate_id(
        &delegation.delegation_id,
        delegation.revision,
        &request.request_id,
        &item.item_id,
        &review_row.action_sha256,
    )
    .map_err(|_| invalid())?;
    let expected_source =
        permission_review_source_id(&request.request_id, &item.item_id).map_err(|_| invalid())?;
    let decision: ApprovalReviewDecision =
        serde_json::from_str(review_row.decision_json.as_deref().ok_or_else(invalid)?)
            .map_err(|_| invalid())?;
    if review_row.status != APPROVAL_REVIEW_STATUS_APPROVED
        || review_row.source_kind != APPROVAL_REVIEW_SOURCE_PERMISSION_ITEM
        || review_row.source_id != expected_source
        || review_row.expires_at <= i64::try_from(now_unix_ms).map_err(|_| invalid())?
        || review_row.candidate_id != expected_candidate
        || u64::try_from(review_row.expires_at).map_err(|_| invalid())?
            != review.candidate_expires_at_unix_ms
        || decision.candidate_id != review.candidate_id
        || decision.verdict != ApprovalVerdict::Approve
        || decision.reason_code != review.reason_code
        || decision.reason != review.reason
    {
        return Err(invalid());
    }
    Ok(AiApprovalGrantProvenance {
        delegation_id: delegation.delegation_id,
        delegation_revision: delegation.revision,
        model_config_revision: u64::try_from(config.configuration_revision)
            .map_err(|_| invalid())?,
        candidate_id: review.candidate_id.clone(),
        decision_event_id: event.event.event_id,
        goal_id: goal.as_ref().map(|(id, _, _)| id.clone()),
        goal_revision: goal.as_ref().map(|(_, revision, _)| *revision),
    })
}

fn invalid() -> DbErr {
    DbErr::Custom("AI approval delegation is stale or inconsistent".into())
}

fn as_i64(value: u64) -> Result<i64, DbErr> {
    value.try_into().map_err(|_| invalid())
}

fn project(value: &ApprovalDelegation) -> Result<delegation_row::ActiveModel, DbErr> {
    value.validate().map_err(|_| invalid())?;
    Ok(delegation_row::ActiveModel {
        delegation_id: Set(value.delegation_id.clone()),
        conversation_id: Set(value.conversation_id.clone()),
        actor_id: Set(value.owner_id.clone()),
        device_id: Set(value.device_id.clone()),
        state_json: Set(serde_json::to_string(value).map_err(|_| invalid())?),
        version: Set(as_i64(value.ledger_version)?),
        status: Set(value.status.as_str().into()),
        created_at: Set(as_i64(value.created_at_unix_ms)?),
        updated_at: Set(as_i64(value.created_at_unix_ms)?),
        ..Default::default()
    })
}

pub(crate) fn decode(row: &delegation_row::Model) -> Result<ApprovalDelegation, DbErr> {
    let value: ApprovalDelegation = serde_json::from_str(&row.state_json).map_err(|_| invalid())?;
    value.validate().map_err(|_| invalid())?;
    if row.delegation_id != value.delegation_id
        || row.conversation_id != value.conversation_id
        || row.actor_id != value.owner_id
        || row.device_id != value.device_id
        || row.version != as_i64(value.ledger_version)?
        || row.status != value.status.as_str()
        || row.created_at != as_i64(value.created_at_unix_ms)?
    {
        return Err(invalid());
    }
    Ok(value)
}

pub async fn load_active_for_subject(
    db: &DatabaseConnection,
    conversation_id: &str,
    owner_id: &str,
    device_id: &str,
) -> Result<Option<ApprovalDelegation>, DbErr> {
    let row = delegation_row::Entity::find()
        .filter(delegation_row::Column::ConversationId.eq(conversation_id))
        .filter(delegation_row::Column::ActorId.eq(owner_id))
        .filter(delegation_row::Column::DeviceId.eq(device_id))
        .filter(delegation_row::Column::Status.eq("active"))
        .one(db)
        .await?;
    row.as_ref().map(decode).transpose()
}

/// Assemble a complete pending request from current durable state. The caller
/// must use the same readiness observation for claim, settlement and the final
/// grant transaction; each of those stages independently re-reads the session,
/// delegation, decisions and grants before trusting this transient batch.
#[allow(clippy::too_many_arguments)]
pub async fn prepare_permission_review_batch(
    db: &DatabaseConnection,
    conversation_id: &str,
    owner_id: &str,
    device_id: &str,
    request_id: &str,
    registry: &desk_diagnose_core::provider_registry::ProviderRegistry,
    surface: desk_agent_protocol::capability_provider::ProductSurface,
    readiness_revision: u64,
    now_unix_ms: u64,
) -> Result<Vec<desk_diagnose_core::approval_review::ApprovalReviewCandidate>, DbErr> {
    use desk_diagnose_core::approval_review::{
        PermissionReviewInput, permission_review_candidate, permission_review_input_kind,
        trusted_permission_descriptor,
    };
    if readiness_revision == 0 || now_unix_ms == 0 {
        return Err(invalid());
    }
    let txn = db.begin().await?;
    let (_, session) = session_on(&txn, conversation_id, owner_id, device_id).await?;
    if session.turn_state.is_active() {
        return Err(invalid());
    }
    let request = session
        .permission_requests
        .iter()
        .find(|request| request.request_id == request_id)
        .ok_or_else(invalid)?;
    let delegation_row = delegation_row::Entity::find()
        .filter(delegation_row::Column::ConversationId.eq(conversation_id))
        .filter(delegation_row::Column::ActorId.eq(owner_id))
        .filter(delegation_row::Column::DeviceId.eq(device_id))
        .filter(delegation_row::Column::Status.eq("active"))
        .one(&txn)
        .await?
        .ok_or_else(invalid)?;
    let delegation = decode(&delegation_row)?;
    let config = crate::approval_model_provider::load(&txn).await?;
    let destination = config.destination_identity().map_err(|_| invalid())?;
    let goal_row = goal_row::Entity::find()
        .filter(goal_row::Column::ConversationId.eq(conversation_id))
        .filter(goal_row::Column::ActorId.eq(owner_id))
        .filter(goal_row::Column::DeviceId.eq(device_id))
        .filter(goal_row::Column::Status.is_not_in(["completed", "failed", "cancelled"]))
        .one(&txn)
        .await?;
    let goal = goal_row
        .as_ref()
        .map(crate::agent_goal_store::decode)
        .transpose()?;
    let authority =
        permission_authority_on(&txn, &session, request_id, readiness_revision, now_unix_ms)
            .await?;
    let mut candidates = Vec::with_capacity(request.items.len());
    for item in &request.items {
        let (descriptor_json, risk) =
            trusted_permission_descriptor(registry, item, surface).map_err(|_| invalid())?;
        let input_kind = permission_review_input_kind(item).map_err(|_| invalid())?;
        let candidate = permission_review_candidate(PermissionReviewInput {
            session: &session,
            request,
            item_id: &item.item_id,
            delegation: &delegation,
            goal: goal.as_ref(),
            descriptor_json: &descriptor_json,
            risk,
            input_kind,
            current_authority: authority.clone(),
            now_unix_ms,
        })
        .map_err(|_| invalid())?;
        desk_diagnose_core::approval_egress::authorize_permission_review_egress(
            &candidate,
            &session,
            request,
            goal.as_ref(),
            &delegation,
            &destination,
            now_unix_ms,
        )
        .map_err(|_| invalid())?;
        candidates.push(candidate);
    }
    txn.commit().await?;
    Ok(candidates)
}

pub async fn load_latest_for_subject(
    db: &DatabaseConnection,
    conversation_id: &str,
    owner_id: &str,
    device_id: &str,
) -> Result<Option<ApprovalDelegation>, DbErr> {
    let row = delegation_row::Entity::find()
        .filter(delegation_row::Column::ConversationId.eq(conversation_id))
        .filter(delegation_row::Column::ActorId.eq(owner_id))
        .filter(delegation_row::Column::DeviceId.eq(device_id))
        .order_by_desc(delegation_row::Column::CreatedAt)
        .order_by_desc(delegation_row::Column::Id)
        .one(db)
        .await?;
    row.as_ref().map(decode).transpose()
}

async fn session_on(
    txn: &DatabaseTransaction,
    conversation_id: &str,
    owner_id: &str,
    device_id: &str,
) -> Result<(agent_session::Model, PersistedAgentSession), DbErr> {
    let row = agent_session::Entity::find()
        .filter(agent_session::Column::ConversationId.eq(conversation_id))
        .filter(agent_session::Column::ActorId.eq(owner_id))
        .filter(agent_session::Column::DeviceId.eq(device_id))
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    let mut session = PersistedAgentSession::decode_json(&row.state_json).map_err(|_| invalid())?;
    if session.conversation_id != conversation_id
        || session.actor_id != owner_id
        || session.device_id != device_id
        || session.surface != AgentSessionSurface::AiAssistant
        || row.version < 0
    {
        return Err(invalid());
    }
    session.version = row.version;
    Ok((row, session))
}

async fn append_audit_on(
    txn: &DatabaseTransaction,
    row: &agent_session::Model,
    session: &mut PersistedAgentSession,
    delegation: &ApprovalDelegation,
    kind: AgentRunEventKind,
    owner_decision_id: &str,
    now: DateTime<Utc>,
) -> Result<(), DbErr> {
    session.last_event_seq = session.last_event_seq.checked_add(1).ok_or_else(invalid)?;
    session.version = row.version.checked_add(1).ok_or_else(invalid)?;
    session.updated_at = now.to_rfc3339();
    let event = ApprovalDelegationAuditEvent {
        event: AgentRunEvent {
            schema_version: AGENT_RUN_EVENT_SCHEMA_VERSION,
            event_id: format!("{}-{}", delegation.delegation_id, kind.as_str()),
            run_id: delegation.conversation_id.clone(),
            event_seq: session.last_event_seq,
            input_revision: session.input_revision,
            kind,
            correlation_id: Some(delegation.delegation_id.clone()),
            source_envelope_ids: vec![],
            result_envelope_ids: vec![],
            created_at: session.updated_at.clone(),
        },
        delegation_id: delegation.delegation_id.clone(),
        owner_decision_id: owner_decision_id.to_owned(),
        delegation_revision: delegation.revision,
        status: delegation.status,
    };
    event.validate_for(delegation).map_err(|_| invalid())?;
    let changed = agent_session::Entity::update_many()
        .col_expr(
            agent_session::Column::StateJson,
            Expr::value(session.encode_json_for_storage().map_err(|_| invalid())?),
        )
        .col_expr(agent_session::Column::Version, Expr::value(session.version))
        .col_expr(agent_session::Column::UpdatedAt, Expr::value(now))
        .filter(agent_session::Column::Id.eq(row.id))
        .filter(agent_session::Column::Version.eq(row.version))
        .filter(agent_session::Column::LeaseToken.eq(row.lease_token))
        .exec(txn)
        .await?;
    if changed.rows_affected != 1 {
        return Err(invalid());
    }
    agent_run_event::ActiveModel {
        event_id: Set(event.event.event_id.clone()),
        run_id: Set(event.event.run_id.clone()),
        event_seq: Set(as_i64(event.event.event_seq)?),
        input_revision: Set(as_i64(event.event.input_revision)?),
        kind: Set(event.event.kind.as_str().into()),
        correlation_id: Set(event.event.correlation_id.clone()),
        input_seq: Set(None),
        actor_id: Set(Some(delegation.owner_id.clone())),
        source_envelope_ids_json: Set("[]".into()),
        result_envelope_ids_json: Set("[]".into()),
        payload_json: Set(serde_json::to_string(&event).map_err(|_| invalid())?),
        payload_schema_version: Set(i32::from(AGENT_RUN_EVENT_SCHEMA_VERSION)),
        created_at: Set(now),
        ..Default::default()
    }
    .insert(txn)
    .await?;
    Ok(())
}

async fn current_goal_on(
    txn: &DatabaseTransaction,
    conversation_id: &str,
    owner_id: &str,
    device_id: &str,
) -> Result<Option<(String, u64, u64)>, DbErr> {
    let row = goal_row::Entity::find()
        .filter(goal_row::Column::ConversationId.eq(conversation_id))
        .filter(goal_row::Column::ActorId.eq(owner_id))
        .filter(goal_row::Column::DeviceId.eq(device_id))
        .filter(goal_row::Column::Status.is_not_in(["completed", "failed", "cancelled"]))
        .one(txn)
        .await?;
    row.as_ref()
        .map(|row| {
            let goal = crate::agent_goal_store::decode(row)?;
            Ok((goal.goal_id, goal.goal_revision, goal.deadline_unix_ms))
        })
        .transpose()
}

#[allow(clippy::too_many_arguments)]
pub async fn open_for_subject(
    db: &DatabaseConnection,
    conversation_id: &str,
    owner_id: &str,
    device_id: &str,
    expected_input_revision: u64,
    owner_authorization_id: String,
    now_unix_ms: u64,
) -> Result<ApprovalDelegation, DbErr> {
    let txn = crate::db::begin_write(db, agent_session::Entity).await?;
    let (session_row, mut session) = session_on(&txn, conversation_id, owner_id, device_id).await?;
    if session.input_revision != expected_input_revision || session.turn_state.is_active() {
        return Err(invalid());
    }
    desk_diagnose_core::assistant_policy::require_current_policy(session.policy_revision)
        .map_err(|_| invalid())?;
    let config = crate::approval_model_provider::load(&txn).await?;
    config.destination_identity().map_err(|_| invalid())?;
    let active = delegation_row::Entity::find()
        .filter(delegation_row::Column::ConversationId.eq(conversation_id))
        .filter(delegation_row::Column::Status.eq("active"))
        .one(&txn)
        .await?;
    if active.is_some() {
        return Err(invalid());
    }
    let delegation = ApprovalDelegation::new(
        format!("approval-delegation-{}", uuid::Uuid::new_v4()),
        conversation_id.to_owned(),
        owner_id.to_owned(),
        device_id.to_owned(),
        owner_authorization_id,
        now_unix_ms,
    )
    .map_err(|_| invalid())?;
    project(&delegation)?.insert(&txn).await?;
    let now =
        DateTime::<Utc>::from_timestamp_millis(i64::try_from(now_unix_ms).map_err(|_| invalid())?)
            .ok_or_else(invalid)?;
    append_audit_on(
        &txn,
        &session_row,
        &mut session,
        &delegation,
        AgentRunEventKind::ApprovalDelegationOpened,
        &delegation.owner_authorization_id,
        now,
    )
    .await?;
    txn.commit().await?;
    Ok(delegation)
}

pub async fn close_for_subject(
    db: &DatabaseConnection,
    conversation_id: &str,
    owner_id: &str,
    device_id: &str,
    delegation_id: &str,
    owner_decision_id: &str,
    now_unix_ms: u64,
) -> Result<ApprovalDelegation, DbErr> {
    let txn = crate::db::begin_write(db, delegation_row::Entity).await?;
    let (session_row, mut session) = session_on(&txn, conversation_id, owner_id, device_id).await?;
    let row = delegation_row::Entity::find()
        .filter(delegation_row::Column::DelegationId.eq(delegation_id))
        .filter(delegation_row::Column::ConversationId.eq(conversation_id))
        .filter(delegation_row::Column::ActorId.eq(owner_id))
        .filter(delegation_row::Column::DeviceId.eq(device_id))
        .one(&txn)
        .await?
        .ok_or_else(invalid)?;
    let mut delegation = decode(&row)?;
    delegation
        .close(ApprovalDelegationStatus::Closed)
        .map_err(|_| invalid())?;
    let changed = delegation_row::Entity::update_many()
        .col_expr(
            delegation_row::Column::StateJson,
            Expr::value(serde_json::to_string(&delegation).map_err(|_| invalid())?),
        )
        .col_expr(
            delegation_row::Column::Version,
            Expr::value(as_i64(delegation.ledger_version)?),
        )
        .col_expr(delegation_row::Column::Status, Expr::value("closed"))
        .col_expr(
            delegation_row::Column::UpdatedAt,
            Expr::value(as_i64(now_unix_ms)?),
        )
        .filter(delegation_row::Column::Id.eq(row.id))
        .filter(delegation_row::Column::Version.eq(row.version))
        .filter(delegation_row::Column::Status.eq("active"))
        .exec(&txn)
        .await?;
    if changed.rows_affected != 1 {
        return Err(invalid());
    }
    let now =
        DateTime::<Utc>::from_timestamp_millis(i64::try_from(now_unix_ms).map_err(|_| invalid())?)
            .ok_or_else(invalid)?;
    append_audit_on(
        &txn,
        &session_row,
        &mut session,
        &delegation,
        AgentRunEventKind::ApprovalDelegationClosed,
        owner_decision_id,
        now,
    )
    .await?;
    txn.commit().await?;
    Ok(delegation)
}
