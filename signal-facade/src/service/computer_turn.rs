//! Host-only status queries. No peer relay, grant, or action dispatch.
use crate::model::{
    auth_context::{AuthContext, AuthKind},
    connection::ConnectionState,
    signal::{RemoteDeskTypeEnum, SignalingModel, SignalingType},
};
use desk_agent_protocol::computer_turn::{
    ComputerActionTurnQuery, ComputerActionTurnState, ComputerActionTurnStatus,
};

pub fn query_from_host(
    source: &ConnectionState,
    model: &SignalingModel,
) -> Option<ComputerActionTurnQuery> {
    query_from_identity(
        &source.auth_context,
        source.model.version_info.remote_desk_type,
        model,
    )
}

fn query_from_identity(
    auth: &AuthContext,
    role: RemoteDeskTypeEnum,
    model: &SignalingModel,
) -> Option<ComputerActionTurnQuery> {
    if model.signaling_type != SignalingType::QueryComputerActionTurn
        || model.to_connection_id.is_some()
        || model.response_state.is_some()
        || model.request_id.is_empty()
        || model.request_id.len() > 256
        || model.request_id.chars().any(char::is_control)
        || auth.auth_kind != AuthKind::TokenAuth
        || auth.remote_desk_type != RemoteDeskTypeEnum::Server
        || role != RemoteDeskTypeEnum::Server
    {
        return None;
    }
    let query: ComputerActionTurnQuery = model.get_data().ok()?;
    query.validate().ok()?;
    (auth.user_id?.to_string() == query.actor_id).then_some(query)
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::computer_use::ComputerActionTurnScope;

    #[test]
    fn only_the_authenticated_owner_host_can_query_without_a_peer_destination() {
        let query = ComputerActionTurnQuery {
            actor_id: "7".into(),
            scope: ComputerActionTurnScope {
                conversation_id: "conversation".into(),
                turn_id: "turn".into(),
                input_revision: 1,
                lease_token: 2,
            },
        };
        let model = SignalingModel::new(
            "probe",
            SignalingType::QueryComputerActionTurn,
            None,
            None,
            Some(serde_json::to_value(&query).unwrap()),
            None,
        );
        let auth = AuthContext::token_auth(7, 1, RemoteDeskTypeEnum::Server);
        assert_eq!(
            query_from_identity(&auth, RemoteDeskTypeEnum::Server, &model),
            Some(query)
        );
        for kind in [AuthKind::None, AuthKind::CookieAuth, AuthKind::CodeSession] {
            let mut other = auth.clone();
            other.auth_kind = kind;
            assert!(query_from_identity(&other, RemoteDeskTypeEnum::Server, &model).is_none());
        }
        let mut other = auth.clone();
        other.user_id = Some(8);
        assert!(query_from_identity(&other, RemoteDeskTypeEnum::Server, &model).is_none());
        let mut peer = model.clone();
        peer.to_connection_id = Some("peer".into());
        assert!(query_from_identity(&auth, RemoteDeskTypeEnum::Server, &peer).is_none());
        let mut response = model;
        response.signaling_type = SignalingType::ComputerActionTurnStatus;
        assert!(query_from_identity(&auth, RemoteDeskTypeEnum::Server, &response).is_none());
    }
}

pub async fn reply(
    source: &ConnectionState,
    request_id: &str,
    query: ComputerActionTurnQuery,
    state: ComputerActionTurnState,
) {
    let status = ComputerActionTurnStatus { query, state };
    let Ok(model) = SignalingModel::success_response(
        request_id,
        SignalingType::ComputerActionTurnStatus,
        None,
        Some(source.model.connection_id.clone()),
        Some(&status),
    ) else {
        return;
    };
    if let Ok(text) = serde_json::to_string(&model) {
        let _ = source.session.write().await.text(text).await;
    }
}
