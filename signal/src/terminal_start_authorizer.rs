//! Signal single-account capability-ceiling stamp for `StartTerminal` frames.
//!
//! The remote terminal opens on a **distinct** WS connection that never does a
//! `RequestRemote`, so on the host it carries no admission and no capability
//! ceiling. Without a stamp the host's first door would reject it (a capability
//! frame from an un-admitted connection) or, worse, fall back to the host global
//! ceiling. This authorizer stamps every `StartTerminal` exactly like
//! [`crate::request_remote_authorizer`] stamps a `RequestRemote`, so the host can
//! register the ceiling, record an admission, and index the connection under its
//! grant — making the terminal connection a first-class capability-scoped session.
//!
//! OSS signal is single-account: the one authenticated operator owns every device
//! it reaches, so an owner's `StartTerminal` is stamped with `access_ceiling: None`
//! (full control). A capability-scoped code-session authorizes solely through the
//! grant it redeemed (the browser-supplied `grant_session_id` is only a lookup key;
//! the authorization fact is the server-side principal / target / generation check),
//! so its terminal is stamped with the code's per-code ceiling or default-denied.
//!
//! Shares the owner-detection, audience-resolution, and grant-ceiling helpers with
//! [`crate::request_remote_authorizer`] so a code-session authorizes identically on
//! its control connection and its terminal connection (no drift).

use async_trait::async_trait;
use desk_signal_facade::grant::AccessGrantStore;
use desk_signal_facade::model::connection::{ConnectionState, SharedConnectionMap};
use desk_signal_facade::model::request_remote_authz::{
    AuthorizedTerminalStart, REQUEST_REMOTE_AUTHZ_VERSION, RequestRemoteAuthz,
};
use desk_signal_facade::model::security_settings::SecuritySettings;
use desk_signal_facade::model::signal::SignalingModel;
use desk_signal_facade::model::terminal::StartTerminalSession;
use desk_signal_facade::service::{RequestRemoteOutcome, TerminalStartAuthorizer};
use desk_utils::error::DeskErrorCode;
use std::sync::Arc;

use crate::request_remote_authorizer::{
    DeviceGenerationLookup, REQUEST_REMOTE_AUTHZ_TTL_SECS, owner_actor_user_id,
    resolve_grant_ceiling, resolve_target_audience,
};

/// Build the `Forward` outcome wrapping a `StartTerminal` frame's data in an
/// [`AuthorizedTerminalStart`] stamp. Pure over its inputs (expiry passed in) so it
/// is unit-testable. Returns `Reject` only if the frame has no payload / cannot be
/// encoded.
fn build_stamped_terminal_outcome(
    model: &SignalingModel,
    access_ceiling: Option<SecuritySettings>,
    grant_session_id: Option<String>,
    generation: i64,
    audience: String,
    expires_at_rfc3339: String,
) -> RequestRemoteOutcome {
    let Some(inner) = model.get_raw_data().clone() else {
        return RequestRemoteOutcome::Reject {
            code: DeskErrorCode::INVALID_PARAMS,
            message: "StartTerminal had no payload".to_string(),
        };
    };
    let authz = RequestRemoteAuthz {
        version: REQUEST_REMOTE_AUTHZ_VERSION,
        access_ceiling,
        grant_session_id,
        generation,
        request_id: model.request_id.clone(),
        audience,
        expires_at: Some(expires_at_rfc3339),
    };
    let wrapper = AuthorizedTerminalStart { inner, authz };
    let data = match serde_json::to_value(&wrapper) {
        Ok(v) => v,
        Err(e) => {
            return RequestRemoteOutcome::Reject {
                code: DeskErrorCode::SYSTEM_ERROR,
                message: format!("failed to encode terminal-start authorization: {e}"),
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

/// Signal single-account `StartTerminal` capability-ceiling stamp. Mirrors
/// [`crate::request_remote_authorizer::SignalRequestRemoteAuthorizer`].
pub struct SignalTerminalStartAuthorizer {
    grant_store: Arc<dyn AccessGrantStore>,
    generation_lookup: Arc<dyn DeviceGenerationLookup>,
}

impl SignalTerminalStartAuthorizer {
    pub fn new(
        grant_store: Arc<dyn AccessGrantStore>,
        generation_lookup: Arc<dyn DeviceGenerationLookup>,
    ) -> Self {
        Self {
            grant_store,
            generation_lookup,
        }
    }
}

#[async_trait]
impl TerminalStartAuthorizer for SignalTerminalStartAuthorizer {
    fn authorize<'a>(
        &'a self,
        actor: &'a ConnectionState,
        connection_map: &'a SharedConnectionMap,
        model: &'a SignalingModel,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = RequestRemoteOutcome> + Send + 'a>>
    {
        Box::pin(async move {
            // Target device: resolved from the receiving connection's validated
            // state, never from a control-end self-report.
            let Some(to_id) = model.to_connection_id.clone() else {
                return RequestRemoteOutcome::Reject {
                    code: DeskErrorCode::INVALID_PARAMS,
                    message: "StartTerminal missing target connection".to_string(),
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

            // The cookie-authenticated single account owns every device it reaches
            // → no ceiling (full control). Resolved from the session, so a control
            // end cannot fake it.
            if owner_actor_user_id(&actor.auth_context).is_some() {
                return build_stamped_terminal_outcome(model, None, None, 0, audience, expires_at);
            }

            // A code-session authorizes solely through the grant it redeemed: stamp
            // the code's ceiling after a server-side lookup keyed by its resolved
            // principal and the browser-supplied grant selector.
            if let Some(principal) = actor.auth_context.grant_principal() {
                let grant_session_id = model
                    .get_data::<StartTerminalSession>()
                    .ok()
                    .and_then(|s| s.grant_session_id);
                return match resolve_grant_ceiling(
                    &self.grant_store,
                    &self.generation_lookup,
                    &principal,
                    &audience,
                    grant_session_id.as_deref(),
                )
                .await
                {
                    Some((ceiling, gsid, generation)) => build_stamped_terminal_outcome(
                        model, ceiling, gsid, generation, audience, expires_at,
                    ),
                    None => RequestRemoteOutcome::Reject {
                        code: DeskErrorCode::PERMISSION_ERROR,
                        message: "no valid access grant for this terminal target".to_string(),
                    },
                };
            }

            // Anything else (node token / anonymous) is default-denied.
            RequestRemoteOutcome::Reject {
                code: DeskErrorCode::PERMISSION_ERROR,
                message: "opening a terminal requires an authenticated operator".to_string(),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_signal_facade::grant::{
        GrantPrincipal, GrantSessionRecord, InProcessAccessGrantStore,
    };
    use desk_signal_facade::model::signal::SignalingType;

    /// A `StartTerminal` model carrying the browser-supplied (untrusted) grant
    /// selector in its `StartTerminalSession` payload.
    fn start_terminal_with_grant(grant_session_id: Option<&str>) -> SignalingModel {
        let inner = StartTerminalSession {
            command: "cmd.exe".to_string(),
            device_id: None,
            grant_session_id: grant_session_id.map(str::to_string),
        };
        SignalingModel::new(
            "req-term-1",
            SignalingType::StartTerminal,
            Some("browser-1".to_string()),
            Some("edge-1".to_string()),
            Some(serde_json::to_value(inner).unwrap()),
            None,
        )
    }

    fn code_ceiling() -> SecuritySettings {
        SecuritySettings {
            allow_terminal: Some(true),
            allow_remote_control: None,
            allow_clipboard_sync: None,
            allow_private_screen: None,
            allow_whiteboard: None,
            allow_file_browse: None,
            allow_file_transfer: None,
            approval_timeout: None,
        }
    }

    struct FixedGeneration(Option<i64>);

    #[async_trait]
    impl DeviceGenerationLookup for FixedGeneration {
        async fn current_generation(&self, _client_id: &str) -> Option<i64> {
            self.0
        }
    }

    async fn authorizer_with_grant(
        record: GrantSessionRecord,
        live_generation: Option<i64>,
    ) -> (SignalTerminalStartAuthorizer, String) {
        let store = Arc::new(InProcessAccessGrantStore::new());
        let minted = store.mint(&record, 300).await.unwrap();
        let authz =
            SignalTerminalStartAuthorizer::new(store, Arc::new(FixedGeneration(live_generation)));
        (authz, minted.grant_session_id)
    }

    fn code_record(session: &str, target: &str, generation: i64) -> GrantSessionRecord {
        GrantSessionRecord {
            principal: GrantPrincipal::from_code_session(session),
            target_device: target.to_string(),
            access_ceiling: Some(code_ceiling()),
            generation,
        }
    }

    #[tokio::test]
    async fn code_session_valid_grant_stamps_the_code_ceiling() {
        let (authz, gsid) =
            authorizer_with_grant(code_record("sess-1", "client-x", 0), Some(0)).await;
        let model = start_terminal_with_grant(Some(&gsid));
        let resolved = resolve_grant_ceiling(
            &authz.grant_store,
            &authz.generation_lookup,
            &GrantPrincipal::from_code_session("sess-1"),
            "client-x",
            model
                .get_data::<StartTerminalSession>()
                .unwrap()
                .grant_session_id
                .as_deref(),
        )
        .await;
        assert_eq!(resolved, Some((Some(code_ceiling()), Some(gsid), 0)));
    }

    #[tokio::test]
    async fn code_session_wrong_principal_is_rejected() {
        let (authz, gsid) =
            authorizer_with_grant(code_record("sess-1", "client-x", 0), Some(0)).await;
        let model = start_terminal_with_grant(Some(&gsid));
        let resolved = resolve_grant_ceiling(
            &authz.grant_store,
            &authz.generation_lookup,
            &GrantPrincipal::from_code_session("sess-2"),
            "client-x",
            model
                .get_data::<StartTerminalSession>()
                .unwrap()
                .grant_session_id
                .as_deref(),
        )
        .await;
        assert_eq!(resolved, None);
    }

    #[tokio::test]
    async fn code_session_superseded_generation_is_rejected() {
        let (authz, gsid) =
            authorizer_with_grant(code_record("sess-1", "client-x", 0), Some(1)).await;
        let model = start_terminal_with_grant(Some(&gsid));
        let resolved = resolve_grant_ceiling(
            &authz.grant_store,
            &authz.generation_lookup,
            &GrantPrincipal::from_code_session("sess-1"),
            "client-x",
            model
                .get_data::<StartTerminalSession>()
                .unwrap()
                .grant_session_id
                .as_deref(),
        )
        .await;
        assert_eq!(resolved, None);
    }

    #[test]
    fn owner_stamp_has_no_ceiling_and_no_grant() {
        let model = start_terminal_with_grant(None);
        let outcome = build_stamped_terminal_outcome(
            &model,
            None,
            None,
            0,
            "client-abc".to_string(),
            "2999-01-01T00:00:00Z".to_string(),
        );
        let frame = match outcome {
            RequestRemoteOutcome::Forward(f) => f,
            _ => panic!("expected a stamped Forward outcome"),
        };
        assert_eq!(frame.request_id, "req-term-1");
        assert_eq!(frame.signaling_type, SignalingType::StartTerminal);
        let wrapper: AuthorizedTerminalStart =
            serde_json::from_value(frame.get_raw_data().clone().unwrap()).unwrap();
        assert_eq!(wrapper.authz.access_ceiling, None);
        assert_eq!(wrapper.authz.grant_session_id, None);
        assert_eq!(wrapper.authz.audience, "client-abc");
        assert_eq!(wrapper.authz.request_id, "req-term-1");
        // The inner payload survives the wrap byte-for-byte.
        let inner: StartTerminalSession = serde_json::from_value(wrapper.inner).unwrap();
        assert_eq!(inner.command, "cmd.exe");
        // Validates against the resolved audience + request id, rejected otherwise.
        assert!(
            wrapper
                .authz
                .validate("req-term-1", "client-abc", "2026-01-01T00:00:00Z")
                .is_ok()
        );
        assert!(
            wrapper
                .authz
                .validate("req-term-1", "other-device", "2026-01-01T00:00:00Z")
                .is_err()
        );
    }
}
