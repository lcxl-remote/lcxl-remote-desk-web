//! Signal single-account capability-ceiling stamp for `RequestRemoteAccess` frames.
//!
//! The portable signal server is a `TrustedCentral` upstream to its edge, so the
//! edge drops any bare `RequestRemoteAccess` arriving there (defense against a grant
//! session stripping its stamp to masquerade as an owner). This authorizer stamps
//! every relayed `RequestRemoteAccess` with an [`RequestRemoteAuthz`] so the edge
//! accepts it.
//!
//! OSS signal is single-account: the one authenticated operator is the owner of
//! every device it reaches, so an authenticated `RequestRemoteAccess` is stamped with
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

use async_trait::async_trait;
use desk_signal_facade::grant::{AccessGrantStore, GrantPrincipal};
use desk_signal_facade::model::auth_context::{AuthContext, AuthKind};
use desk_signal_facade::model::connection::{ConnectionState, SharedConnectionMap};
use desk_signal_facade::model::request_remote_authz::{
    ActorAccessSource, ActorSummary, AuthorizedRequestRemote, REQUEST_REMOTE_AUTHZ_VERSION,
    RequestRemoteAuthz,
};
use desk_signal_facade::model::security_settings::SecuritySettings;
use desk_signal_facade::model::signal::{RemoteDeskTypeEnum, RequestRemoteModel, SignalingModel};
use desk_signal_facade::service::{RequestRemoteAuthorizer, RequestRemoteOutcome};
use desk_utils::error::DeskErrorCode;
use std::sync::Arc;

/// Resolves the live generation of an open-source device code by the target's
/// `client_id`, so a superseded (regenerated) code — and any grant minted from an
/// earlier generation — is refused at stamp time. Abstracted behind a trait so the
/// authorizer's decision logic is unit-testable without a live database.
#[async_trait]
pub trait DeviceGenerationLookup: Send + Sync {
    /// The live code generation for the device registered under `client_id`, or
    /// `None` if no such device code exists (target is not a registered device).
    async fn current_generation(&self, client_id: &str) -> Option<i64>;
}

/// Production [`DeviceGenerationLookup`] backed by the signal's SQLite device-code
/// table.
pub struct DbDeviceGenerationLookup {
    db: sea_orm::DatabaseConnection,
}

impl DbDeviceGenerationLookup {
    pub fn new(db: sea_orm::DatabaseConnection) -> Self {
        Self { db }
    }
}

#[async_trait]
impl DeviceGenerationLookup for DbDeviceGenerationLookup {
    async fn current_generation(&self, client_id: &str) -> Option<i64> {
        use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
        crate::entity::device_code::Entity::find()
            .filter(crate::entity::device_code::Column::ClientId.eq(client_id))
            .one(&self.db)
            .await
            .ok()
            .flatten()
            .map(|row| row.generation as i64)
    }
}

/// Validity window of an injected stamp (seconds); matches the AI authorizer's
/// `AUTHZ_TTL_SECS` and doubles as the edge-enforced replay window. Shared with the
/// terminal-start stamp so both capability stamps expire on the same schedule.
pub(crate) const REQUEST_REMOTE_AUTHZ_TTL_SECS: i64 = 300;

/// Resolve the owner (single-account) user id from a sending connection's auth
/// context. Only a cookie-authenticated control end is a valid actor; a token
/// (node) connection or an anonymous one is not. Shared with the terminal-start
/// authorizer so owner detection cannot drift between the two capability stamps.
pub(crate) fn owner_actor_user_id(ctx: &AuthContext) -> Option<i32> {
    if ctx.auth_kind == AuthKind::CookieAuth {
        ctx.user_id
    } else {
        None
    }
}

/// Build the host-visible identity from a server-adjudicated source. Temporary
/// grants intentionally suppress names even if an upstream caller accidentally
/// supplies one.
pub(crate) fn signal_actor_summary(
    access_source: ActorAccessSource,
    trusted_display_name: Option<String>,
) -> ActorSummary {
    ActorSummary {
        display_name: match access_source {
            ActorAccessSource::AuthenticatedAccount => trusted_display_name,
            ActorAccessSource::TemporaryGrant | ActorAccessSource::Unknown => None,
        },
        access_source,
    }
}

/// Resolve the capability ceiling to stamp for a code-session actor against the
/// resolved target `audience`, keyed by the browser-supplied (untrusted)
/// `grant_session_id` selector. The authorization fact is the server-side lookup:
/// the grant's `principal` / `target` / live `generation` must all match. Returns
/// `(ceiling, Some(grant_session_id), generation)` on success, or `None` to reject.
/// Shared by the `RequestRemoteAccess` and `StartTerminal` stamps so a code-session
/// authorizes identically on both connections.
pub(crate) async fn resolve_grant_ceiling(
    grant_store: &Arc<dyn AccessGrantStore>,
    generation_lookup: &Arc<dyn DeviceGenerationLookup>,
    principal: &GrantPrincipal,
    audience: &str,
    grant_session_id: Option<&str>,
) -> Option<(Option<SecuritySettings>, Option<String>, i64)> {
    let grant_session_id = grant_session_id?;
    let record = grant_store.lookup(grant_session_id).await.ok().flatten()?;
    let current_generation = generation_lookup.current_generation(audience).await?;
    let ceiling = record.authorize(principal, audience, current_generation)?;
    Some((
        ceiling,
        Some(grant_session_id.to_string()),
        current_generation,
    ))
}

/// Resolve the audience (target edge `client_id`) from the receiving connection's
/// validated state. The target must be a token-authenticated `Server` carrying a
/// client id; a control end can never satisfy this. Pure over the validated
/// fields so it is unit-testable without a live connection.
pub(crate) fn resolve_target_audience(
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

/// Build the `Forward` outcome wrapping a `RequestRemoteAccess` frame's data with a
/// stamp. Pure over its inputs (expiry passed in) so it is unit-testable. Returns
/// `Reject` only if the frame has no payload / cannot be encoded.
fn build_stamped_outcome(
    model: &SignalingModel,
    access_ceiling: Option<desk_signal_facade::model::security_settings::SecuritySettings>,
    grant_session_id: Option<String>,
    generation: i64,
    actor: ActorSummary,
    audience: String,
    expires_at_rfc3339: String,
) -> RequestRemoteOutcome {
    let Some(inner) = model.get_raw_data().clone() else {
        return RequestRemoteOutcome::Reject {
            code: DeskErrorCode::INVALID_PARAMS,
            message: "RequestRemoteAccess had no payload".to_string(),
        };
    };
    let authz = RequestRemoteAuthz {
        version: REQUEST_REMOTE_AUTHZ_VERSION,
        access_ceiling,
        grant_session_id,
        generation,
        actor,
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

/// Signal single-account `RequestRemoteAccess` capability-ceiling stamp.
///
/// The authenticated single account is the owner of every device it reaches, so
/// its request is stamped with `access_ceiling: None` (full control). A
/// capability-scoped code-session (an anonymous redeemer) instead authorizes
/// solely through the grant it redeemed: its request is stamped with the code's
/// ceiling (`Some(ceiling)`) after a server-side lookup keyed by its resolved
/// principal, or default-denied.
pub struct SignalRequestRemoteAuthorizer {
    grant_store: Arc<dyn AccessGrantStore>,
    generation_lookup: Arc<dyn DeviceGenerationLookup>,
}

impl SignalRequestRemoteAuthorizer {
    pub fn new(
        grant_store: Arc<dyn AccessGrantStore>,
        generation_lookup: Arc<dyn DeviceGenerationLookup>,
    ) -> Self {
        Self {
            grant_store,
            generation_lookup,
        }
    }

    /// Resolve the capability ceiling to stamp for a code-session actor against
    /// the resolved target `audience` (the target's `client_id`), or `None` to
    /// reject. The grant-session selector on the frame is browser-writable and
    /// therefore untrusted; the authorization fact is the server-side lookup keyed
    /// by the actor's resolved principal, the target audience, and the code's live
    /// generation. Returns `(ceiling, Some(grant_session_id))` on success.
    async fn resolve_code_ceiling(
        &self,
        principal: &GrantPrincipal,
        audience: &str,
        model: &SignalingModel,
    ) -> Option<(Option<SecuritySettings>, Option<String>, i64)> {
        let grant_session_id = model
            .get_data::<RequestRemoteModel>()
            .ok()
            .and_then(|m| m.grant_session_id);
        // Echo the live generation into the stamp so the host records it with the
        // grant and can direct-close the session on a later regeneration.
        resolve_grant_ceiling(
            &self.grant_store,
            &self.generation_lookup,
            principal,
            audience,
            grant_session_id.as_deref(),
        )
        .await
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
            // Target device: resolved from the receiving connection's validated
            // state, never from a control-end self-report. Both an owner and a
            // code-session must address a real registered device.
            let Some(to_id) = model.to_connection_id.clone() else {
                return RequestRemoteOutcome::Reject {
                    code: DeskErrorCode::INVALID_PARAMS,
                    message: "RequestRemoteAccess missing target connection".to_string(),
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
            // → no ceiling (full control). The server resolves this from the
            // session, so a control end cannot fake it.
            if owner_actor_user_id(&actor.auth_context).is_some() {
                // Owner session: no ceiling, no grant → never indexed / revoked on
                // the host, so a `0` generation placeholder is never consulted.
                return build_stamped_outcome(
                    model,
                    None,
                    None,
                    0,
                    signal_actor_summary(
                        ActorAccessSource::AuthenticatedAccount,
                        actor.model.version_info.display_name.clone(),
                    ),
                    audience,
                    expires_at,
                );
            }

            // A code-session authorizes solely through the grant it redeemed: stamp
            // the code's ceiling after a server-side lookup keyed by its resolved
            // principal. It can never be mistaken for the owner (it carries no
            // `user_id`), so it is never stamped with full control.
            if let Some(principal) = actor.auth_context.grant_principal() {
                return match self
                    .resolve_code_ceiling(&principal, &audience, model)
                    .await
                {
                    Some((ceiling, grant_session_id, generation)) => build_stamped_outcome(
                        model,
                        ceiling,
                        grant_session_id,
                        generation,
                        signal_actor_summary(
                            ActorAccessSource::TemporaryGrant,
                            actor.model.version_info.display_name.clone(),
                        ),
                        audience,
                        expires_at,
                    ),
                    None => RequestRemoteOutcome::Reject {
                        code: DeskErrorCode::PERMISSION_ERROR,
                        message: "no valid access grant for this target".to_string(),
                    },
                };
            }

            // Anything else (node token / anonymous) is default-denied.
            RequestRemoteOutcome::Reject {
                code: DeskErrorCode::PERMISSION_ERROR,
                message: "remote control requires an authenticated operator".to_string(),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_signal_facade::model::signal::SignalingType;

    use desk_signal_facade::grant::{GrantSessionRecord, InProcessAccessGrantStore};
    use desk_signal_facade::model::security_settings::SecuritySettings;

    fn request_remote(request_id: &str, to: Option<&str>) -> SignalingModel {
        let data = serde_json::to_value(desk_signal_facade::model::signal::RequestRemoteModel {
            purpose: desk_signal_facade::model::signal::RemoteSessionPurpose::RemoteDesktop,
            requested_wayland_control_mode: Some("portal".to_string()),
            ..Default::default()
        })
        .unwrap();
        SignalingModel::new(
            request_id,
            SignalingType::RequestRemoteAccess,
            Some("browser-1".to_string()),
            to.map(str::to_string),
            Some(data),
            None,
        )
    }

    /// A `RequestRemoteAccess` model carrying the browser-supplied (untrusted) grant
    /// selector.
    fn request_remote_with_grant(grant_session_id: Option<&str>) -> SignalingModel {
        let inner = RequestRemoteModel {
            purpose: desk_signal_facade::model::signal::RemoteSessionPurpose::RemoteDesktop,
            grant_session_id: grant_session_id.map(str::to_string),
            ..Default::default()
        };
        SignalingModel::new(
            "req-1",
            SignalingType::RequestRemoteAccess,
            Some("browser-1".to_string()),
            Some("edge-1".to_string()),
            Some(serde_json::to_value(inner).unwrap()),
            None,
        )
    }

    /// A code that permits terminal but leaves the rest unset (prompt).
    fn code_ceiling() -> SecuritySettings {
        SecuritySettings {
            allow_terminal: Some(true),
            allow_remote_control: None,
            allow_clipboard_sync: None,
            allow_private_screen: None,
            allow_whiteboard: None,
            allow_file_browse: None,
            allow_file_delete: None,
            allow_file_transfer: None,
            allow_system_audio_capture: None,
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

    /// Build an authorizer over an in-process store with one minted grant. Returns
    /// the authorizer and the minted `grant_session_id`.
    async fn authorizer_with_grant(
        record: GrantSessionRecord,
        live_generation: Option<i64>,
    ) -> (SignalRequestRemoteAuthorizer, String) {
        let store = Arc::new(InProcessAccessGrantStore::new());
        let minted = store.mint(&record, 300).await.unwrap();
        let authz =
            SignalRequestRemoteAuthorizer::new(store, Arc::new(FixedGeneration(live_generation)));
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
    async fn code_session_with_valid_grant_stamps_the_code_ceiling() {
        let (authz, gsid) =
            authorizer_with_grant(code_record("sess-1", "client-x", 0), Some(0)).await;
        let model = request_remote_with_grant(Some(&gsid));
        let resolved = authz
            .resolve_code_ceiling(
                &GrantPrincipal::from_code_session("sess-1"),
                "client-x",
                &model,
            )
            .await;
        // Stamps the code's ceiling (not full control), echoes the grant id, and
        // reports the live generation (0) for the host to record with the grant.
        assert_eq!(resolved, Some((Some(code_ceiling()), Some(gsid), 0)));
    }

    #[tokio::test]
    async fn code_session_wrong_principal_is_rejected() {
        let (authz, gsid) =
            authorizer_with_grant(code_record("sess-1", "client-x", 0), Some(0)).await;
        let model = request_remote_with_grant(Some(&gsid));
        // A different code-session presenting someone else's grant id is refused.
        let resolved = authz
            .resolve_code_ceiling(
                &GrantPrincipal::from_code_session("sess-2"),
                "client-x",
                &model,
            )
            .await;
        assert_eq!(resolved, None);
    }

    #[tokio::test]
    async fn code_session_wrong_target_is_rejected() {
        let (authz, gsid) =
            authorizer_with_grant(code_record("sess-1", "client-x", 0), Some(0)).await;
        let model = request_remote_with_grant(Some(&gsid));
        let resolved = authz
            .resolve_code_ceiling(
                &GrantPrincipal::from_code_session("sess-1"),
                "client-other",
                &model,
            )
            .await;
        assert_eq!(resolved, None);
    }

    #[tokio::test]
    async fn code_session_superseded_generation_is_rejected() {
        // Grant minted at generation 0; the live code has been regenerated to 1.
        let (authz, gsid) =
            authorizer_with_grant(code_record("sess-1", "client-x", 0), Some(1)).await;
        let model = request_remote_with_grant(Some(&gsid));
        let resolved = authz
            .resolve_code_ceiling(
                &GrantPrincipal::from_code_session("sess-1"),
                "client-x",
                &model,
            )
            .await;
        assert_eq!(resolved, None);
    }

    #[tokio::test]
    async fn code_session_missing_or_unknown_grant_is_rejected() {
        let (authz, _gsid) =
            authorizer_with_grant(code_record("sess-1", "client-x", 0), Some(0)).await;
        let principal = GrantPrincipal::from_code_session("sess-1");
        // No selector on the frame → reject.
        let no_selector = request_remote_with_grant(None);
        assert_eq!(
            authz
                .resolve_code_ceiling(&principal, "client-x", &no_selector)
                .await,
            None
        );
        // Selector points to no known grant → reject.
        let unknown = request_remote_with_grant(Some("nonexistent-grant"));
        assert_eq!(
            authz
                .resolve_code_ceiling(&principal, "client-x", &unknown)
                .await,
            None
        );
    }

    #[tokio::test]
    async fn code_session_missing_live_device_is_rejected() {
        // The target has no device-code row (generation lookup returns None).
        let (authz, gsid) = authorizer_with_grant(code_record("sess-1", "client-x", 0), None).await;
        let model = request_remote_with_grant(Some(&gsid));
        let resolved = authz
            .resolve_code_ceiling(
                &GrantPrincipal::from_code_session("sess-1"),
                "client-x",
                &model,
            )
            .await;
        assert_eq!(resolved, None);
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
    fn temporary_grant_suppresses_any_supplied_display_name() {
        let actor = signal_actor_summary(
            ActorAccessSource::TemporaryGrant,
            Some("spoofed-owner".to_string()),
        );
        assert_eq!(actor.display_name, None);
        assert_eq!(actor.access_source, ActorAccessSource::TemporaryGrant);
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
            0,
            ActorSummary::unknown(),
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
        assert_eq!(wrapper.inner, model.get_raw_data().clone().unwrap());
        assert_eq!(wrapper.inner["requested_wayland_control_mode"], "portal");
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
            SignalingType::RequestRemoteAccess,
            Some("browser-1".to_string()),
            Some("edge-1".to_string()),
            None,
            None,
        );
        let outcome = build_stamped_outcome(
            &model,
            None,
            None,
            0,
            ActorSummary::unknown(),
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
