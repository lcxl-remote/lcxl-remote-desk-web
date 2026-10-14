//! OSS owner delegation projection. All writes are subject-scoped SQLite
//! transactions; a saved switch is not a grant and cannot dispatch a tool.

use crate::config::connection::{DatabaseConnection, DatabaseTransaction};
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
    ActiveModelTrait, ColumnTrait, DbErr, EntityTrait, QueryFilter, QueryOrder, QuerySelect, Set,
    TransactionTrait,
};

/// A claimed review is the sole permission to make one external reviewer call.
/// The raw candidate is never stored a second time in SQLite.
pub struct ClaimedPermissionReview {
    pub statistics:
        std::sync::Arc<std::sync::Mutex<desk_diagnose_core::approval_observation::ReviewTracking>>,
    pub call_authority: desk_diagnose_core::subagent::reservation::ReviewCallAuthority,
    pub delegation_call:
        Option<desk_diagnose_core::subagent::reservation::DelegationCallReservation>,
    pub candidate_id: String,
    pub lease_epoch: u64,
    pub lease_owner: String,
    pub lease_deadline_unix_ms: u64,
    pub model_destination: desk_agent_protocol::data_lineage::DestinationIdentity,
    pub model_config_revision: u64,
    pub authorized: desk_diagnose_core::approval_egress::AuthorizedApprovalReview,
}

#[derive(Debug)]
pub enum ReviewStartError {
    Unavailable(desk_diagnose_core::approval_observation::ReviewReason),
    Backend(DbErr),
}
impl From<DbErr> for ReviewStartError {
    fn from(error: DbErr) -> Self {
        Self::Backend(error)
    }
}

pub struct PendingPermissionReview {
    pub conversation_id: String,
    pub owner_id: String,
    pub device_id: String,
    pub request_id: String,
    pub directory_request: bool,
}

/// Page active delegations rather than scanning unbounded conversation JSON.
/// This only discovers work; preparation and claim repeat all authority checks.
pub async fn pending_permission_reviews(
    db: &DatabaseConnection,
    after_id: i64,
    limit: u64,
    now_unix_ms: u64,
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
        let owner_id = row.actor_id.clone();
        let mut conversations = vec![row.conversation_id.clone()];
        // Unfinished children are bounded by the root capacity, independent of
        // the main turn's active/idle state and the time the switch was enabled.
        let children = crate::entity::agent_subagent_run::Entity::find()
            .filter(
                crate::entity::agent_subagent_run::Column::RootConversationId
                    .eq(&row.conversation_id),
            )
            .filter(crate::entity::agent_subagent_run::Column::ActorId.eq(&owner_id))
            .filter(crate::entity::agent_subagent_run::Column::DeviceId.eq(&row.device_id))
            .filter(crate::entity::agent_subagent_run::Column::State.is_not_in([
                "completed",
                "failed",
                "cancelled",
            ]))
            .order_by_asc(crate::entity::agent_subagent_run::Column::Id)
            .limit(desk_diagnose_core::subagent::SUBAGENT_ROOT_CAPACITY as u64)
            .all(db)
            .await?;
        conversations.extend(
            children
                .into_iter()
                .map(|child| child.child_conversation_id),
        );
        for conversation_id in conversations {
            let session_row = agent_session::Entity::find()
                .filter(agent_session::Column::ConversationId.eq(&conversation_id))
                .filter(agent_session::Column::ActorId.eq(&owner_id))
                .filter(agent_session::Column::DeviceId.eq(&row.device_id))
                .one(db)
                .await?;
            let Some(session_row) = session_row else {
                continue;
            };
            let session = PersistedAgentSession::decode_json(&session_row.state_json)
                .map_err(|_| invalid())?;
            if session.surface != AgentSessionSurface::AiAssistant
                || !session.allows_delegated_review()
                || session.approval_delegation_conversation_id() != row.conversation_id
                || !crate::agent_subagent_store::shared_approval_current_on(
                    db,
                    &session,
                    as_i64(now_unix_ms)?,
                )
                .await?
            {
                continue;
            }
            for record in session.file_scope.records() {
                if desk_diagnose_core::directory_review::pending(
                    &session,
                    &record.proposal.request_id,
                )
                .is_ok()
                {
                    let delegation = decode(&row)?;
                    let tracking = desk_diagnose_core::approval_observation::ReviewTracking::queued(
                        &owner_id,
                        &row.device_id,
                        &session.conversation_id,
                        &row.conversation_id,
                        &delegation.delegation_id,
                        delegation.revision,
                        "directory_proposal",
                        &record.proposal.request_id,
                        now_unix_ms as i64,
                    );
                    crate::approval_metrics::submit(
                        &tracking,
                        desk_diagnose_core::model_observability::ObservationPhase::Started,
                        0,
                        now_unix_ms as i64,
                    );
                    pending.push(PendingPermissionReview {
                        conversation_id: session.conversation_id.clone(),
                        owner_id: row.actor_id.clone(),
                        device_id: row.device_id.clone(),
                        request_id: record.proposal.request_id.clone(),
                        directory_request: true,
                    });
                }
            }
            if session.turn_state.is_active() {
                continue;
            }
            for request in &session.permission_requests {
                if request.state == PermissionRequestState::Pending
                    && request.input_revision == session.input_revision
                {
                    if !desk_diagnose_core::approval_review::requires_owner_decision(request) {
                        let delegation = decode(&row)?;
                        for item in &request.items {
                            if let Ok(source) =
                                desk_diagnose_core::approval_review::permission_review_source_id(
                                    &request.request_id,
                                    &item.item_id,
                                )
                            {
                                let tracking=desk_diagnose_core::approval_observation::ReviewTracking::queued(&row.actor_id.clone(),&row.device_id,&session.conversation_id,&row.conversation_id,&delegation.delegation_id,delegation.revision,"permission_item",&source,now_unix_ms as i64);
                                crate::approval_metrics::submit(&tracking,desk_diagnose_core::model_observability::ObservationPhase::Started,0,now_unix_ms as i64);
                            }
                        }
                    }
                    pending.push(PendingPermissionReview {
                        conversation_id: session.conversation_id.clone(),
                        owner_id: row.actor_id.clone(),
                        device_id: row.device_id.clone(),
                        request_id: request.request_id.clone(),
                        directory_request: false,
                    });
                }
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
) -> Result<Option<ClaimedPermissionReview>, ReviewStartError> {
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
        return Err(invalid().into());
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
            return Err(invalid().into());
        }
        txn.commit().await?;
        crate::approval_metrics::committed(db, &candidate.candidate_id);
        return Ok(None);
    }
    let config = crate::approval_model_provider::load(&txn).await?;
    let destination = config.destination_identity().map_err(|_| {
        ReviewStartError::Unavailable(
            desk_diagnose_core::approval_observation::ReviewReason::Configuration,
        )
    })?;
    let delegation_row = delegation_row::Entity::find()
        .filter(delegation_row::Column::DelegationId.eq(&candidate.delegation_id))
        .filter(
            delegation_row::Column::ConversationId
                .eq(session.approval_delegation_conversation_id()),
        )
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
    let authorized = if matches!(
        candidate.source,
        desk_diagnose_core::approval_review::ApprovalSource::DirectoryProposal { .. }
    ) {
        desk_diagnose_core::directory_review::authorize(
            candidate,
            &session,
            goal.as_ref(),
            &delegation,
            &destination,
            now_unix_ms,
        )
        .map_err(|_| {
            ReviewStartError::Unavailable(
                desk_diagnose_core::approval_observation::ReviewReason::Policy,
            )
        })?
    } else {
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
            desk_diagnose_core::approval_review::ApprovalSource::PermissionItem {
                item_id, ..
            } => item_id,
            _ => return Err(invalid().into()),
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
            return Err(invalid().into());
        }
        if !desk_diagnose_core::approval_review::current_permission_authority_matches(
            candidate,
            &permission_authority_on(
                &txn,
                &session,
                &request.request_id,
                readiness_revision,
                now_unix_ms,
            )
            .await?,
        ) {
            return Err(invalid().into());
        }
        authorize_permission_review_egress(
            candidate,
            &session,
            request,
            goal.as_ref(),
            &delegation,
            &destination,
            now_unix_ms,
        )
        .map_err(|_| {
            ReviewStartError::Unavailable(
                desk_diagnose_core::approval_observation::ReviewReason::Policy,
            )
        })?
    };
    let reserved_tokens = desk_diagnose_core::approval_usage::reviewer_token_reservation(
        &authorized.prompt,
        u64::try_from(config.gateway.max_context_bytes.ok_or_else(invalid)?)
            .map_err(|_| invalid())?,
        u64::try_from(config.gateway.runtime_max_output_tokens).map_err(|_| invalid())?,
    )
    .ok_or_else(invalid)?;
    delegation.reserve_tokens(reserved_tokens).map_err(|_| {
        ReviewStartError::Unavailable(
            desk_diagnose_core::approval_observation::ReviewReason::Budget,
        )
    })?;
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
        return Err(invalid().into());
    }
    let lease_deadline_unix_ms = now_unix_ms
        .saturating_add(desk_diagnose_core::approval_review::APPROVAL_REVIEW_LEASE_MS)
        .min(candidate.expires_at_unix_ms);
    let (source_kind, source_id) = match &candidate.source {
        desk_diagnose_core::approval_review::ApprovalSource::PermissionItem {
            request_id,
            item_id,
        } => (
            APPROVAL_REVIEW_SOURCE_PERMISSION_ITEM,
            permission_review_source_id(request_id, item_id).map_err(|_| invalid())?,
        ),
        desk_diagnose_core::approval_review::ApprovalSource::DirectoryProposal { request_id } => {
            ("directory_proposal", request_id.clone())
        }
        _ => return Err(invalid().into()),
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
        source_kind: Set(source_kind.into()),
        source_id: Set(source_id),
        action_sha256: Set(candidate.action_sha256.clone()),
        context_hmac_sha256: Set(context_hmac),
        decision_json: Set(None),
        status: Set(APPROVAL_REVIEW_STATUS_REVIEWING.into()),
        lease_epoch: Set(1),
        lease_owner: Set(Some(lease_owner.to_owned())),
        lease_deadline: Set(Some(as_i64(lease_deadline_unix_ms)?)),
        reserved_tokens: Set(as_i64(reserved_tokens)?),
        expires_at: Set(as_i64(candidate.expires_at_unix_ms)?),
        created_at: Set(as_i64(now_unix_ms)?),
        updated_at: Set(as_i64(now_unix_ms)?),
        ..Default::default()
    }
    .insert(&txn)
    .await?;
    let (call_authority, delegation_call) = crate::agent_subagent_store::reserve_review_call_on(
        &txn,
        candidate,
        &destination,
        &authorized,
        reserved_tokens,
        u64::try_from(config.gateway.runtime_max_output_tokens).map_err(|_| invalid())?,
        i64::try_from(now_unix_ms).map_err(|_| invalid())?,
    )
    .await?;
    txn.commit().await?;
    Ok(Some(ClaimedPermissionReview {
        statistics: std::sync::Arc::new(std::sync::Mutex::new(
            desk_diagnose_core::approval_observation::ReviewTracking::new(
                &call_authority,
                now_unix_ms as i64,
            ),
        )),
        call_authority,
        delegation_call,
        candidate_id: candidate.candidate_id.clone(),
        lease_epoch: 1,
        lease_owner: lease_owner.to_owned(),
        lease_deadline_unix_ms,
        model_destination: destination,
        model_config_revision: u64::try_from(config.configuration_revision)
            .map_err(|_| invalid())?,
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
    readiness_revision: u64,
    now_unix_ms: u64,
    tracking: Option<desk_diagnose_core::approval_observation::ReviewTracking>,
) -> Result<String, DbErr> {
    use desk_diagnose_core::approval_egress::validate_permission_review_current;
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
    {
        return Err(invalid());
    }
    let (session_row, session) = session_on(
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
    let model_revision_matches = config.configuration_revision == row.model_config_revision;
    let authority_matches = match request {
        Some(request) if readiness_revision != 0 => permission_authority_on(
            &txn,
            &session,
            &request.request_id,
            readiness_revision,
            now_unix_ms,
        )
        .await
        .is_ok_and(|facts| {
            desk_diagnose_core::approval_review::current_permission_authority_matches(
                candidate, &facts,
            )
        }),
        _ => false,
    };
    let authorized = if matches!(candidate.source, ApprovalSource::DirectoryProposal { .. }) {
        let lease_current = u64::try_from(session_row.lease_token).ok()
            == Some(session.lease_token)
            && session_row
                .lease_deadline
                .is_some_and(|deadline| deadline.timestamp_millis() > now_unix_ms as i64)
            && crate::agent_subagent_store::shared_approval_current_on(
                &txn,
                &session,
                as_i64(now_unix_ms)?,
            )
            .await?;

        lease_current
            && config
                .destination_identity()
                .ok()
                .filter(|_| model_revision_matches)
                .is_some_and(|_destination| {
                    desk_diagnose_core::directory_review::validate_current(
                        candidate,
                        &session,
                        goal.as_ref(),
                        &delegation,
                        now_unix_ms,
                    )
                    .is_ok()
                })
    } else {
        match (request, config.destination_identity()) {
            (Some(request), Ok(_destination)) if model_revision_matches => {
                authority_matches
                    && validate_permission_review_current(
                        candidate,
                        &session,
                        request,
                        goal.as_ref(),
                        &delegation,
                        now_unix_ms,
                    )
                    .is_ok()
            }
            _ => false,
        }
    };
    let now_i64 = as_i64(now_unix_ms)?;
    let expired = now_unix_ms >= candidate.expires_at_unix_ms
        || row
            .lease_deadline
            .is_some_and(|deadline| deadline <= now_i64);
    let physical = crate::agent_subagent_store::settle_review_call_on(
        &txn,
        &row,
        actual_tokens,
        i64::try_from(now_unix_ms).map_err(|_| invalid())?,
    )
    .await?;
    let actual_tokens = physical.tokens;
    let accounted = physical.settlement(row.reserved_tokens)?;
    let usage_in_bounds = actual_tokens.is_none_or(|used| used <= row.reserved_tokens as u64);
    let valid_decision = physical.dispatched
        && decision.is_some_and(|review| review.validate_for(candidate).is_ok());
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
    if physical.dispatched {
        delegation
            .settle_tokens(row.reserved_tokens as u64, Some(accounted.tokens))
            .map_err(|_| invalid())?;
    } else {
        delegation
            .release_unstarted_review_tokens(row.reserved_tokens as u64)
            .map_err(|_| invalid())?;
    }
    crate::agent_approval_usage::record_usage_on(
        &txn,
        &row,
        accounted,
        i64::try_from(now_unix_ms).map_err(|_| invalid())?,
    )
    .await?;
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
            .map(serde_json::to_string)
            .transpose()
            .map_err(|_| invalid())?
    } else {
        None
    };
    let observation_reason = if delegation.status
        != desk_diagnose_core::approval_delegation::ApprovalDelegationStatus::Active
    {
        Some(desk_diagnose_core::approval_observation::ReviewReason::Revoked)
    } else if now_unix_ms >= candidate.expires_at_unix_ms {
        Some(desk_diagnose_core::approval_observation::ReviewReason::CandidateExpired)
    } else if row.lease_deadline.is_some_and(|v| v <= now_unix_ms as i64) {
        Some(desk_diagnose_core::approval_observation::ReviewReason::LeaseExpired)
    } else if !authorized {
        Some(desk_diagnose_core::approval_observation::ReviewReason::SourceChanged)
    } else if !usage_in_bounds {
        Some(desk_diagnose_core::approval_observation::ReviewReason::Budget)
    } else {
        None
    };
    let statistics_json = crate::approval_metrics::settled_json(
        &row,
        tracking,
        status,
        observation_reason,
        now_unix_ms as i64,
    );
    let directory_failure = statistics_json
        .as_deref()
        .and_then(|value| {
            serde_json::from_str::<desk_diagnose_core::approval_observation::ReviewTracking>(value)
                .ok()
        })
        .and_then(|value| value.snapshot.reason);
    let changed = review_row::Entity::update_many()
        .col_expr(
            review_row::Column::StatisticsJson,
            Expr::value(statistics_json),
        )
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
    commit_directory_on(&txn, &row, status, decision, directory_failure, now_unix_ms).await?;
    txn.commit().await?;
    crate::approval_metrics::committed(db, &candidate.candidate_id);
    Ok(status.to_owned())
}

/// A crashed or timed-out reviewer cannot redial the same candidate. Preserve
/// unknown sent usage, release proven unsent allocations and close the verdict
/// lease. Late usage remains accounting evidence, never a renewed verdict.
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
            .filter(delegation_row::Column::ActorId.eq(&row.actor_id))
            .filter(delegation_row::Column::DeviceId.eq(&row.device_id))
            .one(&txn)
            .await?
            .ok_or_else(invalid)?;
        let mut delegation = decode(&stored)?;
        if row.reserved_tokens <= 0 {
            return Err(invalid());
        }
        let physical =
            crate::agent_subagent_store::settle_review_call_on(&txn, &row, None, now).await?;
        let accounted = physical.settlement(row.reserved_tokens)?;
        if physical.dispatched {
            delegation
                .settle_tokens(row.reserved_tokens as u64, Some(accounted.tokens))
                .map_err(|_| invalid())?;
        } else {
            delegation
                .release_unstarted_review_tokens(row.reserved_tokens as u64)
                .map_err(|_| invalid())?;
        }
        crate::agent_approval_usage::record_usage_on(&txn, &row, accounted, now).await?;
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
        let reason = if delegation.status
            != desk_diagnose_core::approval_delegation::ApprovalDelegationStatus::Active
        {
            desk_diagnose_core::approval_observation::ReviewReason::Revoked
        } else if row.expires_at <= now {
            desk_diagnose_core::approval_observation::ReviewReason::CandidateExpired
        } else {
            desk_diagnose_core::approval_observation::ReviewReason::LeaseExpired
        };
        let statistics_json =
            crate::approval_metrics::settled_json(&row, None, status, Some(reason), now);
        let directory_failure = statistics_json
            .as_deref()
            .and_then(|value| {
                serde_json::from_str::<desk_diagnose_core::approval_observation::ReviewTracking>(
                    value,
                )
                .ok()
            })
            .and_then(|value| value.snapshot.reason);
        let changed = review_row::Entity::update_many()
            .col_expr(
                review_row::Column::StatisticsJson,
                Expr::value(statistics_json),
            )
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
        commit_directory_on(&txn, &row, status, None, directory_failure, now_unix_ms).await?;
        txn.commit().await?;
        crate::approval_metrics::committed(db, &row.candidate_id);
        settled += 1;
    }
    Ok(settled)
}

pub(crate) struct ReviewedPermissionBatch {
    pub decisions: Vec<desk_diagnose_core::dynamic_run::PermissionDecisionItem>,
    pub source: PermissionDecisionSource,
    pub delegation: ApprovalDelegation,
    pub goal_binding: Option<(String, u64)>,
}

/// A competing coordinator must not turn an in-flight review into a fault
/// denial. The caller holds the session write transaction while checking this
/// before compiling a final automatic decision.
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
#[allow(clippy::too_many_arguments)]
pub(crate) async fn reviewed_permission_batch_on(
    txn: &DatabaseTransaction,
    session: &PersistedAgentSession,
    request: &desk_diagnose_core::dynamic_run::PermissionRequest,
    candidates: &[desk_diagnose_core::approval_review::ApprovalReviewCandidate],
    unstarted_failures: &[desk_diagnose_core::dynamic_run::SystemPermissionDecisionEvidence],
    registry: &desk_diagnose_core::provider_registry::ProviderRegistry,
    surface: desk_agent_protocol::capability_provider::ProductSurface,
    hmac_key: &[u8; 32],
    readiness_revision: u64,
    now_unix_ms: u64,
) -> Result<ReviewedPermissionBatch, DbErr> {
    use desk_diagnose_core::approval_egress::validate_permission_review_current;
    use desk_diagnose_core::approval_review::{
        ApprovalReviewDecision, ApprovalVerdict, candidate_context_hmac_sha256,
        permission_decision_from_reviews,
    };
    if candidates.len() != request.items.len() || candidates.is_empty() {
        return Err(invalid());
    }
    if !crate::agent_subagent_store::shared_approval_current_on(txn, session, as_i64(now_unix_ms)?)
        .await?
    {
        return Err(invalid());
    }
    if has_live_permission_review_on(txn, session, request, now_unix_ms).await? {
        return Err(DbErr::Custom(
            "AI permission review is still in progress".into(),
        ));
    }
    let first = &candidates[0];
    let stored = delegation_row::Entity::find()
        .filter(delegation_row::Column::DelegationId.eq(&first.delegation_id))
        .filter(
            delegation_row::Column::ConversationId
                .eq(session.approval_delegation_conversation_id()),
        )
        .filter(delegation_row::Column::ActorId.eq(&session.actor_id))
        .filter(delegation_row::Column::DeviceId.eq(&session.device_id))
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    let delegation = decode(&stored)?;
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
    let mut failures = Vec::new();
    let mut model_revisions = std::collections::BTreeMap::new();
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
        if !desk_diagnose_core::approval_review::current_permission_authority_matches(
            candidate,
            &current_authority,
        ) {
            return Err(invalid());
        }
        validate_permission_review_current(
            candidate,
            session,
            request,
            goal.as_ref(),
            &delegation,
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
            .await?;
        let Some(row) = row else {
            let failure = unstarted_failures
                .iter()
                .find(|failure| {
                    failure.item_id == *item_id
                        && failure.candidate_id.as_deref() == Some(candidate.candidate_id.as_str())
                })
                .ok_or_else(invalid)?;
            failures.push(failure.clone());
            continue;
        };
        let expected_hmac =
            candidate_context_hmac_sha256(hmac_key, candidate).map_err(|_| invalid())?;
        if row.delegation_id != delegation.delegation_id
            || row.model_config_revision <= 0
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
        if matches!(row.status.as_str(), "unavailable" | "expired") {
            let reason = row
                .statistics_json
                .as_deref()
                .and_then(|json| {
                    serde_json::from_str::<
                            desk_diagnose_core::approval_observation::ReviewTracking,
                        >(json)
                        .ok()
                })
                .and_then(|tracking| tracking.snapshot.reason)
                .unwrap_or(desk_diagnose_core::approval_observation::ReviewReason::Processing);
            let (reason_code, reason_text) =
                desk_diagnose_core::approval_review::system_review_reason(reason);
            failures.push(
                desk_diagnose_core::dynamic_run::SystemPermissionDecisionEvidence {
                    item_id: item_id.clone(),
                    candidate_id: Some(candidate.candidate_id.clone()),
                    reason_code: reason_code.into(),
                    reason: reason_text.into(),
                },
            );
            continue;
        }
        model_revisions.insert(
            candidate.candidate_id.clone(),
            u64::try_from(row.model_config_revision).map_err(|_| invalid())?,
        );
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
    let (decisions, source) = permission_decision_from_reviews(
        session,
        request,
        &delegation,
        candidates,
        &reviews,
        &failures,
        &model_revisions,
    )
    .map_err(|_| invalid())?;
    Ok(ReviewedPermissionBatch {
        decisions,
        source,
        delegation,
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
    if !crate::agent_subagent_store::shared_approval_current_on(txn, session, as_i64(now_unix_ms)?)
        .await?
    {
        return Err(invalid());
    }
    let row = delegation_row::Entity::find()
        .filter(delegation_row::Column::DelegationId.eq(&parent.delegation_id))
        .filter(
            delegation_row::Column::ConversationId
                .eq(session.approval_delegation_conversation_id()),
        )
        .filter(delegation_row::Column::ActorId.eq(&session.actor_id))
        .filter(delegation_row::Column::DeviceId.eq(&session.device_id))
        .one(txn)
        .await?
        .ok_or_else(invalid)?;
    let delegation = decode(&row)?;
    let goal = current_goal_on(
        txn,
        &session.conversation_id,
        &session.actor_id,
        &session.device_id,
    )
    .await?;
    delegation
        .require_current_for_session(session)
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
    let PermissionDecisionSource::AutomaticReview {
        delegation_id,
        delegation_revision,
        reviews,
        ..
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
    if review_row.model_config_revision
        != i64::try_from(review.model_config_revision).map_err(|_| invalid())?
        || review_row.status != APPROVAL_REVIEW_STATUS_APPROVED
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
        model_config_revision: u64::try_from(review_row.model_config_revision)
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
    let session = agent_session::Entity::find()
        .filter(agent_session::Column::ConversationId.eq(conversation_id))
        .filter(agent_session::Column::ActorId.eq(owner_id.to_owned()))
        .filter(agent_session::Column::DeviceId.eq(device_id))
        .one(db)
        .await?;
    let session = session
        .as_ref()
        .map(|row| PersistedAgentSession::decode_json(&row.state_json).map_err(|_| invalid()))
        .transpose()?;
    if let Some(child) = session
        .as_ref()
        .filter(|session| !session.agent_role.is_main())
        && !crate::agent_subagent_store::shared_approval_current_on(
            db,
            child,
            chrono::Utc::now().timestamp_millis(),
        )
        .await?
    {
        return Ok(None);
    }
    let conversation_id = session.as_ref().map_or(conversation_id, |session| {
        session.approval_delegation_conversation_id()
    });
    let row = delegation_row::Entity::find()
        .filter(delegation_row::Column::ConversationId.eq(conversation_id))
        .filter(delegation_row::Column::ActorId.eq(owner_id))
        .filter(delegation_row::Column::DeviceId.eq(device_id))
        .filter(delegation_row::Column::Status.eq("active"))
        .one(db)
        .await?;
    row.as_ref().map(decode).transpose()
}

/// Restore content-free evidence selection and verify the frozen review digest.
async fn restore_review_context_on(
    txn: &DatabaseTransaction,
    candidate: &mut desk_diagnose_core::approval_review::ApprovalReviewCandidate,
    session: &PersistedAgentSession,
) -> Result<bool, DbErr> {
    let row = review_row::Entity::find()
        .filter(review_row::Column::CandidateId.eq(&candidate.candidate_id))
        .one(txn)
        .await?;
    let Some(row) = row else { return Ok(false) };
    let authority: desk_diagnose_core::subagent::reservation::ReviewCallAuthority =
        serde_json::from_str(row.call_authority_json.as_deref().ok_or_else(invalid)?)
            .map_err(|_| invalid())?;
    authority
        .context_binding
        .restore(candidate, session)
        .map_err(|_| invalid())?;
    let key = crate::approval_review_secret::load(txn)
        .await?
        .ok_or_else(invalid)?;
    if authority.candidate_id != candidate.candidate_id
        || desk_diagnose_core::approval_review::candidate_context_hmac_sha256(&key, candidate)
            .map_err(|_| invalid())?
            != row.context_hmac_sha256
    {
        return Err(invalid());
    }
    Ok(true)
}

/// Assemble pending items from current durable state; resume claimed items
/// using their original evidence selection. Every commit checks current authority.
#[allow(clippy::too_many_arguments)]
pub async fn prepare_permission_review_batch(
    db: &DatabaseConnection,
    conversation_id: &str,
    owner_id: &str,
    device_id: &str,
    request_id: &str,
    directory_request: bool,
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
    if (!directory_request && session.turn_state.is_active())
        || !crate::agent_subagent_store::shared_approval_current_on(
            &txn,
            &session,
            as_i64(now_unix_ms)?,
        )
        .await?
    {
        return Err(invalid());
    }
    let delegation_row = delegation_row::Entity::find()
        .filter(
            delegation_row::Column::ConversationId
                .eq(session.approval_delegation_conversation_id()),
        )
        .filter(delegation_row::Column::ActorId.eq(owner_id))
        .filter(delegation_row::Column::DeviceId.eq(device_id))
        .filter(delegation_row::Column::Status.eq("active"))
        .one(&txn)
        .await?
        .ok_or_else(invalid)?;
    let delegation = decode(&delegation_row)?;
    let configured = crate::approval_model_provider::load(&txn).await?;
    let budget = usize::try_from(configured.gateway.max_context_bytes.ok_or_else(invalid)?)
        .map_err(|_| invalid())?;
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
    if directory_request {
        let mut candidate = desk_diagnose_core::directory_review::candidate(
            &session,
            request_id,
            &delegation,
            goal.as_ref(),
            now_unix_ms,
        )
        .map_err(|_| invalid())?;
        if !restore_review_context_on(&txn, &mut candidate, &session).await? {
            desk_diagnose_core::approval_review::fit_review_context(
                &mut candidate,
                &session,
                budget,
            )
            .map_err(|_| invalid())?;
        }
        txn.commit().await?;
        return Ok(vec![candidate]);
    }
    let request = session
        .permission_requests
        .iter()
        .find(|request| request.request_id == request_id)
        .ok_or_else(invalid)?;
    // An interactive command needs the owner's live terminal at approval time,
    // so AI auto-approval never decides it; the request stays with the owner.
    if desk_diagnose_core::approval_review::requires_owner_decision(request) {
        txn.commit().await?;
        return Ok(Vec::new());
    }
    let authority =
        permission_authority_on(&txn, &session, request_id, readiness_revision, now_unix_ms)
            .await?;
    let mut candidates = Vec::with_capacity(request.items.len());
    for item in &request.items {
        let (descriptor_json, risk) =
            trusted_permission_descriptor(registry, item, surface).map_err(|_| invalid())?;
        let input_kind = permission_review_input_kind(item).map_err(|_| invalid())?;
        let mut candidate = permission_review_candidate(PermissionReviewInput {
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
        if !restore_review_context_on(&txn, &mut candidate, &session).await? {
            desk_diagnose_core::approval_review::fit_review_context(
                &mut candidate,
                &session,
                budget,
            )
            .map_err(|_| invalid())?;
        }
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
    let session = agent_session::Entity::find()
        .filter(agent_session::Column::ConversationId.eq(conversation_id))
        .filter(agent_session::Column::ActorId.eq(owner_id.to_owned()))
        .filter(agent_session::Column::DeviceId.eq(device_id))
        .one(db)
        .await?;
    let session = session
        .as_ref()
        .map(|row| PersistedAgentSession::decode_json(&row.state_json).map_err(|_| invalid()))
        .transpose()?;
    if let Some(child) = session
        .as_ref()
        .filter(|session| !session.agent_role.is_main())
        && !crate::agent_subagent_store::shared_approval_current_on(
            db,
            child,
            chrono::Utc::now().timestamp_millis(),
        )
        .await?
    {
        return Ok(None);
    }
    let conversation_id = session.as_ref().map_or(conversation_id, |session| {
        session.approval_delegation_conversation_id()
    });
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
    if !session.agent_role.is_main()
        || session.input_revision != expected_input_revision
        || session.turn_state.is_active()
    {
        return Err(invalid());
    }
    desk_diagnose_core::assistant_policy::require_approval_policy(&session)
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
    let delegation = ApprovalDelegation::new_usage_only(
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

/// Apply only the still-waiting directory, without issuing any capability grant.
async fn commit_directory_on(
    txn: &DatabaseTransaction,
    row: &review_row::Model,
    status: &str,
    review: Option<&desk_diagnose_core::approval_review::ApprovalReviewDecision>,
    failure_reason: Option<desk_diagnose_core::approval_observation::ReviewReason>,
    now: u64,
) -> Result<(), DbErr> {
    use desk_diagnose_core::directory_review::DirectoryDecision;
    if row.source_kind != "directory_proposal" {
        return Ok(());
    }
    let (session_row, session) =
        session_on(txn, &row.conversation_id, &row.actor_id, &row.device_id).await?;
    if desk_diagnose_core::directory_review::pending(&session, &row.source_id).is_err() {
        return Ok(());
    }
    let authority: desk_diagnose_core::subagent::reservation::ReviewCallAuthority =
        serde_json::from_str(row.call_authority_json.as_deref().ok_or_else(invalid)?)
            .map_err(|_| invalid())?;
    if session.input_revision != authority.input_revision
        || session.control_revision != authority.control_revision
        || Some(session.lease_token) != authority.source_planning_lease_token
        || u64::try_from(session_row.lease_token).ok() != authority.source_planning_lease_token
    {
        return Ok(());
    }
    let approved = status == APPROVAL_REVIEW_STATUS_APPROVED;
    let model_decision =
        approved || status == desk_diagnose_core::approval_review::APPROVAL_REVIEW_STATUS_DENIED;
    let decision = if model_decision {
        let review = review.ok_or_else(invalid)?;
        DirectoryDecision {
            source: PermissionDecisionSource::AutomaticReview {
                failures: vec![],
                delegation_id: row.delegation_id.clone(),
                delegation_revision: authority.delegation_revision,
                reviews: vec![
                    desk_diagnose_core::dynamic_run::AiPermissionDecisionEvidence {
                        model_config_revision: u64::try_from(row.model_config_revision)
                            .map_err(|_| invalid())?,
                        item_id: row.source_id.clone(),
                        candidate_id: row.candidate_id.clone(),
                        candidate_expires_at_unix_ms: u64::try_from(row.expires_at)
                            .map_err(|_| invalid())?,
                        reason_code: review.reason_code.clone(),
                        reason: review.reason.clone(),
                    },
                ],
            },
            reason: review.reason.clone(),
            candidate_id: Some(row.candidate_id.clone()),
        }
    } else {
        let reason = desk_diagnose_core::directory_review::failure_reason(failure_reason);
        DirectoryDecision {
            source: PermissionDecisionSource::SystemDecision {
                reason_code: "directory_review_unavailable".into(),
                reason: reason.into(),
            },
            reason: reason.into(),
            candidate_id: Some(row.candidate_id.clone()),
        }
    };
    save_directory_decision_on(
        txn,
        &session_row,
        &session,
        &row.source_id,
        approved,
        decision,
        now,
    )
    .await
}

async fn save_directory_decision_on(
    txn: &DatabaseTransaction,
    session_row: &agent_session::Model,
    session: &PersistedAgentSession,
    request_id: &str,
    approved: bool,
    decision: desk_diagnose_core::directory_review::DirectoryDecision,
    now: u64,
) -> Result<(), DbErr> {
    use desk_diagnose_core::file_scope::transaction::FILE_SCOPE_EVENT_KIND;
    let (mut next, receipt) = desk_diagnose_core::file_scope::transaction::prepare_ai_decision(
        session, request_id, approved, decision, now,
    )
    .map_err(|_| invalid())?;
    let timestamp = DateTime::<Utc>::from_timestamp_millis(as_i64(now)?).ok_or_else(invalid)?;
    let timestamp = timestamp.max(session_row.updated_at);
    next.updated_at = timestamp.to_rfc3339();
    let changed = agent_session::Entity::update_many()
        .col_expr(
            agent_session::Column::StateJson,
            Expr::value(next.encode_json_for_storage().map_err(|_| invalid())?),
        )
        .col_expr(agent_session::Column::Version, Expr::value(next.version))
        .col_expr(agent_session::Column::UpdatedAt, Expr::value(timestamp))
        .filter(agent_session::Column::Id.eq(session_row.id))
        .filter(agent_session::Column::Version.eq(session_row.version))
        .filter(agent_session::Column::LeaseToken.eq(session_row.lease_token))
        .exec(txn)
        .await?;
    if changed.rows_affected != 1 {
        return Err(invalid());
    }
    agent_run_event::ActiveModel {
        event_id: Set(receipt.event_id.clone()),
        run_id: Set(next.conversation_id.clone()),
        event_seq: Set(as_i64(receipt.event_seq)?),
        input_revision: Set(as_i64(next.input_revision)?),
        kind: Set(FILE_SCOPE_EVENT_KIND.into()),
        actor_id: Set(Some(next.actor_id.clone())),
        correlation_id: Set(Some(receipt.update.client_request_id.clone())),
        input_seq: Set(None),
        source_envelope_ids_json: Set("[]".into()),
        result_envelope_ids_json: Set("[]".into()),
        payload_json: Set(serde_json::to_string(&receipt).map_err(|_| invalid())?),
        payload_schema_version: Set(1),
        created_at: Set(timestamp),
        ..Default::default()
    }
    .insert(txn)
    .await?;
    Ok(())
}

/// End an unreviewable proposal without leaving the running tool waiting forever.
pub async fn deny_directory_unavailable(
    db: &DatabaseConnection,
    conversation: &str,
    owner: &str,
    device: &str,
    request_id: &str,
    now: u64,
    only_expired: bool,
) -> Result<(), DbErr> {
    let txn = crate::db::begin_write(db, agent_session::Entity).await?;
    let (session_row, session) = session_on(&txn, conversation, owner, device).await?;
    if desk_diagnose_core::directory_review::pending(&session, request_id).is_err()
        || !crate::agent_subagent_store::shared_approval_current_on(&txn, &session, as_i64(now)?)
            .await?
    {
        return Ok(());
    }
    let record = desk_diagnose_core::directory_review::pending(&session, request_id)
        .map_err(|_| invalid())?;
    let expired = desk_diagnose_core::directory_review::deadline(record)
        .is_ok_and(|deadline| now >= deadline);
    if only_expired && !expired {
        return Ok(());
    }
    if review_row::Entity::find()
        .filter(review_row::Column::ConversationId.eq(conversation))
        .filter(review_row::Column::DeviceId.eq(device))
        .filter(review_row::Column::SourceKind.eq("directory_proposal"))
        .filter(review_row::Column::SourceId.eq(request_id))
        .filter(
            review_row::Column::Status
                .eq(desk_diagnose_core::approval_review::APPROVAL_REVIEW_STATUS_REVIEWING),
        )
        .one(&txn)
        .await?
        .is_some()
    {
        return Ok(());
    }
    let Some(row) = delegation_row::Entity::find()
        .filter(
            delegation_row::Column::ConversationId
                .eq(session.approval_delegation_conversation_id()),
        )
        .filter(delegation_row::Column::Status.eq("active"))
        .one(&txn)
        .await?
    else {
        return Ok(());
    };
    let delegation = decode(&row)?;
    delegation
        .require_current_for_session(&session)
        .map_err(|_| invalid())?;
    let reason_category = crate::approval_metrics::unavailable_reason_on(&txn).await;
    let reason_category = if expired {
        desk_diagnose_core::approval_observation::ReviewReason::CandidateExpired
    } else {
        reason_category
    };
    let reason = desk_diagnose_core::directory_review::failure_reason(Some(reason_category));
    let decision = desk_diagnose_core::directory_review::DirectoryDecision {
        source: PermissionDecisionSource::SystemDecision {
            reason_code: "directory_review_unavailable".into(),
            reason: reason.into(),
        },
        reason: reason.into(),
        candidate_id: None,
    };
    save_directory_decision_on(
        &txn,
        &session_row,
        &session,
        request_id,
        false,
        decision,
        now,
    )
    .await?;
    txn.commit().await?;
    let tracking = desk_diagnose_core::approval_observation::ReviewTracking::queued(
        owner,
        device,
        conversation,
        session.approval_delegation_conversation_id(),
        &delegation.delegation_id,
        delegation.revision,
        "directory_proposal",
        request_id,
        now as i64,
    )
    .settled("unavailable", Some(reason_category), now as i64);
    crate::approval_metrics::submit(
        &tracking,
        desk_diagnose_core::model_observability::ObservationPhase::Terminal,
        0,
        now as i64,
    );
    Ok(())
}

struct CapacityContext {
    delegation: ApprovalDelegation,
    goal: Option<desk_diagnose_core::goal::GoalRun>,
    budget: usize,
}

async fn capacity_context(
    db: &DatabaseConnection,
    session: &PersistedAgentSession,
) -> Result<Option<CapacityContext>, desk_agent_protocol::AgentError> {
    let failure = || desk_agent_protocol::AgentError {
        kind: desk_agent_protocol::AgentErrorKind::Internal,
        message: "Approval capacity configuration could not be read".into(),
        retryable: true,
        safe_for_model: true,
        error_code: None,
    };
    let Some(delegation) = load_active_for_subject(
        db,
        &session.conversation_id,
        &session.actor_id,
        &session.device_id,
    )
    .await
    .map_err(|_| failure())?
    else {
        return Ok(None);
    };
    if delegation.status
        != desk_diagnose_core::approval_delegation::ApprovalDelegationStatus::Active
    {
        return Ok(None);
    }
    let configured = crate::approval_model_provider::load(db)
        .await
        .map_err(|_| failure())?;
    let budget = usize::try_from(configured.gateway.max_context_bytes.ok_or_else(failure)?)
        .map_err(|_| failure())?;
    let goal = goal_row::Entity::find()
        .filter(goal_row::Column::ConversationId.eq(&session.conversation_id))
        .filter(goal_row::Column::ActorId.eq(&session.actor_id))
        .filter(goal_row::Column::DeviceId.eq(&session.device_id))
        .filter(goal_row::Column::Status.is_not_in(["completed", "failed", "cancelled"]))
        .one(db)
        .await
        .map_err(|_| failure())?;
    let goal = goal
        .as_ref()
        .map(crate::agent_goal_store::decode)
        .transpose()
        .map_err(|_| failure())?;
    Ok(Some(CapacityContext {
        delegation,
        goal,
        budget,
    }))
}

pub async fn validate_permission_capacity(
    db: &DatabaseConnection,
    session: &PersistedAgentSession,
    request: &desk_diagnose_core::dynamic_run::PermissionRequest,
    registry: &desk_diagnose_core::provider_registry::ProviderRegistry,
) -> Result<(), desk_agent_protocol::AgentError> {
    let Some(context) = capacity_context(db, session).await? else {
        return Ok(());
    };
    desk_diagnose_core::approval_review::preflight_review_capacity(
        session,
        request,
        &context.delegation,
        context.goal.as_ref(),
        registry,
        desk_agent_protocol::capability_provider::ProductSurface::OssPersonalOwner,
        context.budget,
    )
    .map_err(desk_diagnose_core::approval_review::capacity_error)
}

pub async fn validate_directory_capacity(
    db: &DatabaseConnection,
    session: &PersistedAgentSession,
    proposal: desk_diagnose_core::file_scope::DirectoryProposal,
    now_unix_ms: u64,
) -> Result<(), desk_agent_protocol::AgentError> {
    let Some(context) = capacity_context(db, session).await? else {
        return Ok(());
    };
    desk_diagnose_core::directory_review::preflight_capacity(
        session,
        proposal,
        &context.delegation,
        context.goal.as_ref(),
        context.budget,
        now_unix_ms,
    )
    .map_err(desk_diagnose_core::approval_review::capacity_error)
}
