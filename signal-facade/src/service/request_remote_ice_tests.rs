use super::*;

#[test]
fn remote_access_initialized_business_error_is_forwarded_without_success_payload() {
    let response = SignalingModel::error(
        "request-1",
        SignalingType::RemoteAccessInitialized,
        Some("host-1".to_string()),
        Some("browser-1".to_string()),
        DeskErrorCode::ACTION_NEED_RETRY,
        "credential proof pending",
    )
    .unwrap();

    let rebuilt = rebuild_remote_access_initialized_with_ice(&response, None).unwrap();
    assert_eq!(
        rebuilt.signaling_type,
        SignalingType::RemoteAccessInitialized
    );
    assert_eq!(rebuilt.request_id, "request-1");
    assert!(rebuilt.get_raw_data().is_none());
    assert_eq!(
        rebuilt.response_state.unwrap().error_code,
        DeskErrorCode::ACTION_NEED_RETRY.code()
    );
}

/// Fake provider that records nothing — `get_ice_servers` must never be hit
/// (REQUEST_REMOTE must use the REST path), and `get_rest_ice_servers`
/// echoes the requested name into the username so the test can assert the
/// recipient identity was used.
struct FakeTurn {
    issue: bool,
}
#[async_trait::async_trait]
impl TurnProvider for FakeTurn {
    async fn get_ice_servers(&self, _username: &str, _credential: &str) -> LcxlRTCIceServer {
        unreachable!("REQUEST_REMOTE injection must use get_rest_ice_servers")
    }
    async fn get_rest_ice_servers(&self, name: &str, _ttl: u64) -> Option<LcxlRTCIceServer> {
        self.issue.then(|| LcxlRTCIceServer {
            urls: vec!["turn:host:3478?transport=udp".to_string()],
            username: format!("9999999999:{name}"),
            credential: "pw".to_string(),
        })
    }
}

fn model(to: Option<&str>) -> SignalingModel {
    SignalingModel::new(
        "req-1",
        SignalingType::RequestRemoteAccess,
        None,
        to.map(str::to_string),
        None,
        None,
    )
}

fn initialized_model(to: Option<&str>) -> SignalingModel {
    SignalingModel::new(
        "req-1",
        SignalingType::RemoteAccessInitialized,
        None,
        to.map(str::to_string),
        None,
        None,
    )
}

fn provider(issue: bool) -> Arc<dyn TurnProvider> {
    Arc::new(FakeTurn { issue })
}

#[tokio::test]
async fn injects_for_recipient_via_trait_object() {
    let turn = provider(true);
    let ice = build_request_remote_ice(&model(Some("host-1")), "browser-conn", 7, Some(&turn), 60)
        .await
        .expect("ice server");
    // Username embeds the RECIPIENT id, proving recipient identity is used
    // (not the sender `browser-conn`), through the trait-object override.
    assert!(ice.username.ends_with(":host-1"));
}

#[test]
fn session_request_uses_authenticated_controller_not_inbound_sender() {
    let mut inbound = model(Some("host-1"));
    inbound.from_connection_id = Some("forged-browser".into());
    let request = build_remote_session_request(&inbound, "authenticated-browser", 7, 60)
        .expect("session request");
    assert_eq!(request.controller_connection_id, "authenticated-browser");
    assert_eq!(request.host_connection_id, "host-1");
}

#[tokio::test]
async fn none_without_recipient() {
    let turn = provider(true);
    assert!(
        build_request_remote_ice(&model(None), "browser-conn", 7, Some(&turn), 60)
            .await
            .is_none()
    );
}

#[tokio::test]
async fn none_without_turn() {
    assert!(
        build_request_remote_ice(&model(Some("host-1")), "browser-conn", 7, None, 60)
            .await
            .is_none()
    );
}

#[tokio::test]
async fn none_when_provider_declines() {
    let turn = provider(false);
    assert!(
        build_request_remote_ice(&model(Some("host-1")), "browser-conn", 7, Some(&turn), 60,)
            .await
            .is_none()
    );
}

#[test]
fn peer_request_uses_authenticated_host_when_inbound_sender_is_absent() {
    let request = build_remote_session_peer_request(
        &initialized_model(Some("browser-conn")),
        "authenticated-host",
        Some("legacy-client-id".into()),
        60,
    )
    .expect("peer request");
    assert_eq!(request.controller_connection_id, "browser-conn");
    assert_eq!(request.host_connection_id, "authenticated-host");
}

#[test]
fn turn_rebuild_preserves_requested_wayland_control_mode() {
    let payload = RequestRemoteModel {
        purpose: crate::model::signal::RemoteSessionPurpose::RemoteDesktop,
        requested_wayland_control_mode: Some("uinput".to_string()),
        ..RequestRemoteModel::default()
    };
    let original = SignalingModel::new(
        "req-wayland",
        SignalingType::RequestRemoteAccess,
        Some("browser-conn".to_string()),
        Some("host-1".to_string()),
        Some(serde_json::to_value(payload).unwrap()),
        None,
    );
    let ice = LcxlRTCIceServer {
        urls: vec!["turn:host:3478".to_string()],
        username: "host-1".to_string(),
        credential: "secret".to_string(),
    };

    let rebuilt = rebuild_request_remote_with_ice(&original, Some(ice)).unwrap();
    let rebuilt_payload = rebuilt.get_data::<RequestRemoteModel>().unwrap();
    assert_eq!(
        rebuilt_payload.requested_wayland_control_mode.as_deref(),
        Some("uinput")
    );
    assert_eq!(rebuilt_payload.ice_servers.len(), 1);
}

/// Build a REQUEST_REMOTE carrying `payload` so the org-context assertions
/// exercise the same decode path production uses.
fn model_with_payload(to: Option<&str>, payload: RequestRemoteModel) -> SignalingModel {
    SignalingModel::new(
        "req-org",
        SignalingType::RequestRemoteAccess,
        None,
        to.map(str::to_string),
        Some(serde_json::to_value(payload).unwrap()),
        None,
    )
}

/// `org_id` is a browser-supplied *selector*, so signal-facade only forwards it
/// into the TURN session request; a central manager still has to validate
/// membership and the org's device grant before it can pick a payer. Losing it
/// here would silently downgrade an org session to personal context.
#[test]
fn session_request_forwards_browser_org_context() {
    let payload = RequestRemoteModel {
        purpose: crate::model::signal::RemoteSessionPurpose::RemoteDesktop,
        org_id: Some(42),
        ..RequestRemoteModel::default()
    };
    let request = build_remote_session_request(
        &model_with_payload(Some("host-1"), payload),
        "browser",
        7,
        60,
    )
    .expect("session request");
    assert_eq!(request.org_id, Some(42));
    assert_eq!(request.host_connection_id, "host-1");
}

/// A request without `org_id` is personal context. Standalone signal servers
/// never populate it, so `None` must survive as `None` rather than becoming a
/// default org.
#[test]
fn session_request_without_org_id_is_personal_context() {
    let request = build_remote_session_request(&model(Some("host-1")), "browser", 7, 60)
        .expect("session request");
    assert_eq!(request.org_id, None);
}

/// Injecting recipient TURN credentials re-serializes the payload, so every
/// browser-supplied admission field has to survive the round trip — `org_id`
/// included.
#[test]
fn turn_rebuild_preserves_org_id() {
    let payload = RequestRemoteModel {
        purpose: crate::model::signal::RemoteSessionPurpose::RemoteDesktop,
        org_id: Some(7),
        ..RequestRemoteModel::default()
    };
    let ice = LcxlRTCIceServer {
        urls: vec!["turn:host:3478".to_string()],
        username: "host-1".to_string(),
        credential: "secret".to_string(),
    };

    let rebuilt =
        rebuild_request_remote_with_ice(&model_with_payload(Some("host-1"), payload), Some(ice))
            .unwrap();
    let rebuilt_payload = rebuilt.get_data::<RequestRemoteModel>().unwrap();
    assert_eq!(rebuilt_payload.org_id, Some(7));
    assert_eq!(rebuilt_payload.ice_servers.len(), 1);
}
