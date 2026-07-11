//! Signal single-account capability-ceiling stamp for `RequestRemote` frames.
//!
//! The portable signal server is a `TrustedCentral` upstream to its edge, so the
//! edge drops any bare `RequestRemote` arriving there (defense against a grant
//! session stripping its stamp to masquerade as an owner). This authorizer stamps
//! every relayed `RequestRemote` with an [`RequestRemoteAuthz`] so the edge
//! accepts it.
//!
//! OSS signal is single-account: the one authenticated operator is the owner of
//! every device it reaches, so an authenticated `RequestRemote` is stamped with
//! `access_ceiling: None` (full control, no ceiling). Redeemed device / support
//! codes — which stamp `Some(ceiling)` — arrive with the unified redeem flow and
//! its anonymous code-session identity; until that path exists, only the
//! authenticated owner is honored and everything else is default-denied.
//!
//! Trusted-field discipline mirrors [`crate::control_authorizer`]: the actor is
//! the single account (server-resolved from the cookie session, never
//! self-reported) and the audience (target edge `client_id`) is resolved from the
//! **receiving** token-authenticated `Server` connection, so a control end can
//! neither be addressed as a device nor claim to be one.

use desk_signal_facade::model::auth_context::{AuthContext, AuthKind};
use desk_signal_facade::model::connection::{ConnectionState, SharedConnectionMap};
use desk_signal_facade::model::request_remote_authz::{
    AuthorizedRequestRemote, REQUEST_REMOTE_AUTHZ_VERSION, RequestRemoteAuthz,
};
use desk_signal_facade::model::signal::{RemoteDeskTypeEnum, SignalingModel};
use desk_signal_facade::service::{RequestRemoteAuthorizer, RequestRemoteOutcome};
use desk_utils::error::DeskErrorCode;

/// Validity window of an injected stamp (seconds); matches the AI authorizer's
/// `AUTHZ_TTL_SECS` and doubles as the edge-enforced replay window.
const REQUEST_REMOTE_AUTHZ_TTL_SECS: i64 = 300;

/// Resolve the owner (single-account) user id from a sending connection's auth
/// context. Only a cookie-authenticated control end is a valid actor; a token
/// (node) connection or an anonymous one is not.
fn owner_actor_user_id(ctx: &AuthContext) -> Option<i32> {
    if ctx.auth_kind == AuthKind::CookieAuth {
        ctx.user_id
    } else {
        None
    }
}

/// Resolve the audience (target edge `client_id`) from the receiving connection's
/// validated state. The target must be a token-authenticated `Server` carrying a
/// client id; a control end can never satisfy this. Pure over the validated
/// fields so it is unit-testable without a live connection.
fn resolve_target_audience(
    auth_kind: AuthKind,
    remote_desk_type: RemoteDeskTypeEnum,
    client_id: Option<&str>,
) -> Option<String> {
    let is_server =
        auth_kind == AuthKind::TokenAuth && remote_desk_type == RemoteDeskTypeEnum::Server;
    if !is_server {
        return None;
    }
    match client_id {
        Some(id) if !id.is_empty() => Some(id.to_string()),
        _ => None,
    }
}

/// Build the `Forward` outcome wrapping a `RequestRemote` frame's data with a
/// stamp. Pure over its inputs (expiry passed in) so it is unit-testable. Returns
/// `Reject` only if the frame has no payload / cannot be encoded.
fn build_stamped_outcome(
    model: &SignalingModel,
    access_ceiling: Option<desk_signal_facade::model::security_settings::SecuritySettings>,
    grant_session_id: Option<String>,
    audience: String,
    expires_at_rfc3339: String,
) -> RequestRemoteOutcome {
    let Some(inner) = model.get_raw_data().clone() else {
        return RequestRemoteOutcome::Reject {
            code: DeskErrorCode::INVALID_PARAMS,
            message: "RequestRemote had no payload".to_string(),
        };
    };
    let authz = RequestRemoteAuthz {
        version: REQUEST_REMOTE_AUTHZ_VERSION,
        access_ceiling,
        grant_session_id,
        request_id: model.request_id.clone(),
        audience,
        expires_at: Some(expires_at_rfc3339),
    };
    let wrapper = AuthorizedRequestRemote { inner, authz };
    let data = match serde_json::to_value(&wrapper) {
        Ok(v) => v,
        Err(e) => {
            return RequestRemoteOutcome::Reject {
                code: DeskErrorCode::SYSTEM_ERROR,
                message: format!("failed to encode request-remote authorization: {e}"),
            };
        }
    };
    RequestRemoteOutcome::Forward(SignalingModel::new(
        &model.request_id,
        model.signaling_type,
        model.from_connection_id.clone(),
        model.to_connection_id.clone(),
        Some(data),
        model.response_state.clone(),
    ))
}

/// Signal single-account `RequestRemote` capability-ceiling stamp.
#[derive(Default)]
pub struct SignalRequestRemoteAuthorizer;

impl SignalRequestRemoteAuthorizer {
    pub fn new() -> Self {
        Self
    }
}

impl RequestRemoteAuthorizer for SignalRequestRemoteAuthorizer {
    fn authorize<'a>(
        &'a self,
        actor: &'a ConnectionState,
        connection_map: &'a SharedConnectionMap,
        model: &'a SignalingModel,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = RequestRemoteOutcome> + Send + 'a>>
    {
        Box::pin(async move {
            // Actor must be the cookie-authenticated single account (the owner);
            // the server resolves this, the control end cannot fake it. Anonymous
            // requests are default-denied.
            if owner_actor_user_id(&actor.auth_context).is_none() {
                return RequestRemoteOutcome::Reject {
                    code: DeskErrorCode::PERMISSION_ERROR,
                    message: "remote control requires an authenticated operator".to_string(),
                };
            }

            // Target device: resolved from the receiving connection's validated
            // state, never from a control-end self-report.
            let Some(to_id) = model.to_connection_id.clone() else {
                return RequestRemoteOutcome::Reject {
                    code: DeskErrorCode::INVALID_PARAMS,
                    message: "RequestRemote missing target connection".to_string(),
                };
            };
            let audience = {
                let map = connection_map.read().await;
                match map.get(&to_id) {
                    None => None,
                    Some(target) => resolve_target_audience(
                        target.auth_context.auth_kind,
                        target.auth_context.remote_desk_type,
                        target.model.version_info.client_id.as_deref(),
                    ),
                }
            };
            let Some(audience) = audience else {
                return RequestRemoteOutcome::Reject {
                    code: DeskErrorCode::PERMISSION_ERROR,
                    message: "target is not an authorized device".to_string(),
                };
            };

            let expires_at = (chrono::Utc::now()
                + chrono::Duration::seconds(REQUEST_REMOTE_AUTHZ_TTL_SECS))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

            // Single account = owner of every device it reaches → no ceiling.
            build_stamped_outcome(model, None, None, audience, expires_at)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_signal_facade::model::signal::SignalingType;

    fn request_remote(request_id: &str, to: Option<&str>) -> SignalingModel {
        let data =
            serde_json::to_value(desk_signal_facade::model::signal::RequestRemoteModel::default())
                .unwrap();
        SignalingModel::new(
            request_id,
            SignalingType::RequestRemote,
            Some("browser-1".to_string()),
            to.map(str::to_string),
            Some(data),
            None,
        )
    }

    #[test]
    fn owner_actor_only_for_cookie_auth() {
        let cookie = AuthContext::cookie(1, RemoteDeskTypeEnum::Browser);
        assert_eq!(owner_actor_user_id(&cookie), Some(1));
        let token = AuthContext::token_auth(1, 1, RemoteDeskTypeEnum::Server);
        assert_eq!(owner_actor_user_id(&token), None);
        let anon = AuthContext::anonymous(RemoteDeskTypeEnum::Browser);
        assert_eq!(owner_actor_user_id(&anon), None);
    }

    #[test]
    fn target_resolves_only_for_token_server_with_client_id() {
        assert_eq!(
            resolve_target_audience(
                AuthKind::TokenAuth,
                RemoteDeskTypeEnum::Server,
                Some("client-abc"),
            ),
            Some("client-abc".to_string())
        );
        // A control end can never be addressed as a device.
        assert_eq!(
            resolve_target_audience(
                AuthKind::CookieAuth,
                RemoteDeskTypeEnum::Browser,
                Some("client-abc"),
            ),
            None
        );
        // A token Server without a client id has no audience.
        assert_eq!(
            resolve_target_audience(AuthKind::TokenAuth, RemoteDeskTypeEnum::Server, None),
            None
        );
    }

    #[test]
    fn owner_request_is_stamped_with_no_ceiling() {
        let model = request_remote("req-1", Some("edge-1"));
        let outcome = build_stamped_outcome(
            &model,
            None,
            None,
            "client-abc".to_string(),
            "2999-01-01T00:00:00Z".to_string(),
        );
        let frame = match outcome {
            RequestRemoteOutcome::Forward(f) => f,
            _ => panic!("expected a stamped Forward outcome"),
        };
        assert_eq!(frame.request_id, "req-1");
        assert_eq!(frame.to_connection_id.as_deref(), Some("edge-1"));
        let wrapper: AuthorizedRequestRemote =
            serde_json::from_value(frame.get_raw_data().clone().unwrap()).unwrap();
        // Owner session: no ceiling, no revocable grant.
        assert_eq!(wrapper.authz.access_ceiling, None);
        assert_eq!(wrapper.authz.grant_session_id, None);
        assert_eq!(wrapper.authz.audience, "client-abc");
        assert_eq!(wrapper.authz.request_id, "req-1");
        // Validates against the resolved audience + request id, rejected otherwise.
        assert!(
            wrapper
                .authz
                .validate("req-1", "client-abc", "2026-01-01T00:00:00Z")
                .is_ok()
        );
        assert!(
            wrapper
                .authz
                .validate("req-1", "other-device", "2026-01-01T00:00:00Z")
                .is_err()
        );
    }

    #[test]
    fn wrapper_fails_closed_without_payload() {
        let model = SignalingModel::new(
            "req-1",
            SignalingType::RequestRemote,
            Some("browser-1".to_string()),
            Some("edge-1".to_string()),
            None,
            None,
        );
        let outcome = build_stamped_outcome(
            &model,
            None,
            None,
            "client-abc".to_string(),
            "2999-01-01T00:00:00Z".to_string(),
        );
        assert!(matches!(
            outcome,
            RequestRemoteOutcome::Reject {
                code: DeskErrorCode::INVALID_PARAMS,
                ..
            }
        ));
    }
}
