//! Grant scope matching against current task authority in the dispatch transaction.
use crate::entity::agent_session;
use desk_agent_protocol::capability_grant::{CapabilityGrant, CapabilityGrantIssuer};
use desk_diagnose_core::{
    capability_grant::{
        CapabilityGrantCall, match_capability_grant, match_reserved_capability_grant,
        match_reserved_task_capability_grant, match_task_capability_grant,
    },
    session::{PersistedAgentSession, TriggerOrigin},
};
use sea_orm::{ColumnTrait, DatabaseTransaction, DbErr, EntityTrait, QueryFilter};

pub(super) async fn match_current_on(
    txn: &DatabaseTransaction,
    grant: &CapabilityGrant,
    call: &CapabilityGrantCall<'_>,
    reserved: bool,
) -> Result<(), DbErr> {
    let invalid = || DbErr::Custom("capability grant is not authorized by the current run".into());
    let row = agent_session::Entity::find()
        .filter(agent_session::Column::ConversationId.eq(call.run_id))
        .one(txn)
        .await?;
    let session = row
        .as_ref()
        .map(|row| PersistedAgentSession::decode_json(&row.state_json))
        .transpose()
        .map_err(|_| invalid())?;
    let fresh = session
        .as_ref()
        .is_some_and(|session| session.trigger_origin == TriggerOrigin::ScheduledTask);
    let task_grant = matches!(grant.issued_by, CapabilityGrantIssuer::TaskAuthorization(_));
    if fresh || task_grant {
        if !fresh || !task_grant {
            return Err(invalid());
        }
        // Prepare/intent callers acquire task -> session locks before work/grant
        // locks. Rechecking those same held locks here observes revocation and
        // expiry without trusting provenance supplied by the grant as authority.
        let row = crate::schedule_store::lock_action_session(txn, call.run_id)
            .await?
            .ok_or_else(invalid)?;
        let session = PersistedAgentSession::decode_json(&row.state_json).map_err(|_| invalid())?;
        if session.actor_id != call.actor_id || session.device_id != call.target_device_id {
            return Err(invalid());
        }
        let current = crate::schedule_store::fresh_action_authority_on(txn, &session).await?;
        if reserved {
            match_reserved_task_capability_grant(grant, call, current.provenance())
        } else {
            match_task_capability_grant(grant, call, current.provenance())
        }
        .map_err(|_| invalid())
    } else {
        if reserved {
            match_reserved_capability_grant(grant, call)
        } else {
            match_capability_grant(grant, call)
        }
        .map_err(|_| invalid())
    }
}
