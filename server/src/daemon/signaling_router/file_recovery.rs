use super::*;
use desk_agent_protocol::file_recovery::{
    FileRecoveryFailure, FileRecoveryOutcome, FileRecoveryReply, FileRecoveryRequest,
    MANAGEMENT_GRANT,
};

pub(super) async fn handle(ctx: &RouterContext, model: &SignalingModel) -> Result<(), RouterError> {
    // This is a user management operation, not an AI action. It remains available
    // when Device Assistant is disabled so owners can retrieve/clean old backups.
    let reject = |reason| reply_failure(ctx, model, reason);
    let Some(authz) = ctx.inbound_authz.as_ref() else {
        reject(FileRecoveryFailure::Unauthorized);
        return Ok(());
    };
    let Some(actor) = authz.actor.user_id.filter(|id| *id > 0) else {
        reject(FileRecoveryFailure::Unauthorized);
        return Ok(());
    };
    if !authz
        .orchestrator_grants
        .iter()
        .any(|grant| grant == MANAGEMENT_GRANT)
    {
        reject(FileRecoveryFailure::Unauthorized);
        return Ok(());
    }
    let Some(authority) = ctx.file_recovery_authority.as_ref() else {
        reject(FileRecoveryFailure::IdentityChanged);
        return Ok(());
    };
    let request = match model.get_data::<FileRecoveryRequest>() {
        Ok(request) if request.command.validate().is_ok() => request,
        _ => {
            reject(FileRecoveryFailure::InvalidRequest);
            return Ok(());
        }
    };
    let payload = desk_ipc_protocol::message::FileRecoveryRequestPayload {
        request_id: model.request_id.clone(),
        connection_id: None,
        authority: authority.clone(),
        actor_id: actor.to_string(),
        device_id: authz.audience.clone(),
        request,
    };
    if ctx
        .worker_mgr
        .send_file_recovery_request(payload)
        .await
        .is_err()
    {
        reject(FileRecoveryFailure::WorkerUnavailable);
    }
    Ok(())
}

fn reply_failure(ctx: &RouterContext, model: &SignalingModel, reason: FileRecoveryFailure) {
    let reply = FileRecoveryReply {
        authority: ctx
            .file_recovery_authority
            .clone()
            .unwrap_or_else(|| "0".repeat(64)),
        os_user: String::new(),
        outcome: FileRecoveryOutcome::Unavailable { reason },
    };
    if let Ok(frame) = SignalingModel::success_response(
        &model.request_id,
        SignalingType::FileRecoveryManaged,
        None,
        None,
        Some(&reply),
    ) && let Ok(text) = serde_json::to_string(&frame)
    {
        let _ = ctx.outbound_tx.send(text);
    }
}
