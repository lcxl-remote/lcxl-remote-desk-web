//! Single-instance enumeration intersects the live map with cookie authority.

use std::sync::Arc;

use actix_web::web;
use desk_signal_facade::{
    error::DeskSignalFacadeError,
    grant::AccessGrantStore,
    model::{
        auth_context::AuthKind,
        code_session::CodeSessionCookie,
        connection::{
            ConnectionModel, ConnectionState, FetchConnectionsScope, SharedConnectionMap,
        },
        signal::RemoteDeskTypeEnum,
    },
    service::FetchConnectionsResolver,
};
use desk_utils::error::DeskErrorCode;

use crate::request_remote_authorizer::{DeviceGenerationLookup, resolve_grant_ceiling};

pub(crate) struct SignalFetchConnectionsResolver {
    connections: web::Data<SharedConnectionMap>,
    code_session: Option<CodeSessionCookie>,
    grants: Arc<dyn AccessGrantStore>,
    generations: Arc<dyn DeviceGenerationLookup>,
}

impl SignalFetchConnectionsResolver {
    pub(crate) fn new(
        connections: web::Data<SharedConnectionMap>,
        code_session: Option<CodeSessionCookie>,
        grants: Arc<dyn AccessGrantStore>,
        generations: Arc<dyn DeviceGenerationLookup>,
    ) -> Self {
        Self {
            connections,
            code_session,
            grants,
            generations,
        }
    }
}

fn is_server(state: &ConnectionState) -> bool {
    state.auth_context.auth_kind == AuthKind::TokenAuth
        && state.auth_context.remote_desk_type == RemoteDeskTypeEnum::Server
}

impl FetchConnectionsResolver for SignalFetchConnectionsResolver {
    fn resolve<'a>(
        &'a self,
        requester: &'a ConnectionState,
        _scope: FetchConnectionsScope,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<Vec<ConnectionModel>, DeskSignalFacadeError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let denied = || {
                DeskSignalFacadeError::new_custom_error(
                    DeskErrorCode::PERMISSION_ERROR,
                    "Connection listing is not permitted",
                )
            };
            let auth = &requester.auth_context;
            if auth.remote_desk_type != RemoteDeskTypeEnum::Browser {
                return Err(denied());
            }
            if auth.auth_kind == AuthKind::CookieAuth
                && auth.user_id == Some(crate::control_authorizer::SINGLE_ACCOUNT_USER_ID)
            {
                let snapshots: Vec<_> = {
                    let connections = self.connections.read().await;
                    connections
                        .values()
                        .filter(|state| is_server(state))
                        .map(|state| state.model.clone())
                        .collect()
                };
                return Ok(snapshots
                    .into_iter()
                    .map(|model| model.as_ref().clone())
                    .collect());
            }
            let cookie = self.code_session.as_ref().ok_or_else(denied)?;
            let principal = auth.grant_principal().ok_or_else(denied)?;
            if auth.code_session_id.as_deref() != Some(cookie.code_session_id.as_str()) {
                return Err(denied());
            }
            // Snapshot just the authorized candidate, releasing the map before
            // the grant and generation reads. Never clone another device.
            let target = {
                let connections = self.connections.read().await;
                connections
                    .get(&cookie.target_connection_id)
                    .filter(|state| is_server(state))
                    .map(ConnectionState::public_model)
            }
            .ok_or_else(denied)?;
            let audience = target
                .device_id
                .as_deref()
                .filter(|id| !id.is_empty())
                .ok_or_else(denied)?;
            if resolve_grant_ceiling(
                &self.grants,
                &self.generations,
                &principal,
                audience,
                Some(&cookie.grant_session_id),
            )
            .await
            .is_none()
            {
                return Err(denied());
            }
            Ok(vec![target])
        })
    }
}

#[cfg(test)]
mod tests;
