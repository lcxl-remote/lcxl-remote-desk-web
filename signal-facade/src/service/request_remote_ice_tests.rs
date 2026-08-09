use super::*;

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
        SignalingType::RequestRemote,
        Some("browser-conn".to_string()),
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
    let ice = build_request_remote_ice(&model(Some("host-1")), Some(&turn), 60)
        .await
        .expect("ice server");
    // Username embeds the RECIPIENT id, proving recipient identity is used
    // (not the sender `browser-conn`), through the trait-object override.
    assert!(ice.username.ends_with(":host-1"));
}

#[tokio::test]
async fn none_without_recipient() {
    let turn = provider(true);
    assert!(
        build_request_remote_ice(&model(None), Some(&turn), 60)
            .await
            .is_none()
    );
}

#[tokio::test]
async fn none_without_turn() {
    assert!(
        build_request_remote_ice(&model(Some("host-1")), None, 60)
            .await
            .is_none()
    );
}

#[tokio::test]
async fn none_when_provider_declines() {
    let turn = provider(false);
    assert!(
        build_request_remote_ice(&model(Some("host-1")), Some(&turn), 60)
            .await
            .is_none()
    );
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
        SignalingType::RequestRemote,
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
