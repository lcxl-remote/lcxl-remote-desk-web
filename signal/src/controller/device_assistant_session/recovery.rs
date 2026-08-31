//! Offline recovery locates original state; it never authorizes a new action.
use super::*;

pub(crate) async fn resolve(
    store: &SignalAgentSessionStore,
    connections: &SharedConnectionMap,
    actor: &str,
    connection: &str,
    session: Option<&str>,
    conversation: Option<&str>,
) -> Result<Option<(String, String)>, DeskSignalError> {
    let live = {
        let map = connections.read().await;
        match map.get(connection) {
            Some(target) => {
                if target.auth_context.auth_kind != AuthKind::TokenAuth
                    || target.auth_context.remote_desk_type != RemoteDeskTypeEnum::Server
                {
                    return Ok(None);
                }
                let Some(audience) = target
                    .model
                    .version_info
                    .client_id
                    .as_deref()
                    .filter(|id| !id.is_empty())
                else {
                    return Ok(None);
                };
                Some(audience.to_owned())
            }
            None => None,
        }
    };
    let session = session.filter(|id| !id.is_empty());
    let device = if let Some(device) = live {
        device
    } else {
        let Some(run) = session else { return Ok(None) };
        let Some(device) = store.recovery_device(run, actor).await.map_err(|_| {
            DeskSignalError::new_custom_error(
                DeskErrorCode::PERMISSION_ERROR,
                "Device Assistant session not found or not accessible",
            )
        })?
        else {
            return Ok(None);
        };
        device
    };
    let run = match (session, conversation) {
        (Some(run), _) => run.to_owned(),
        (None, Some(intent)) => derive_conversation_key(actor, &device, Some(intent), ""),
        _ => return Ok(None),
    };
    Ok(Some((run, device)))
}
