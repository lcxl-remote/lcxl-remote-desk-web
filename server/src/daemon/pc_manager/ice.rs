//! ICE server filtering and daemon PeerConnection construction.

use std::collections::HashSet;
use std::time::Duration;

use desk_signal_facade::model::signal::{LcxlRTCIceServer, TurnTransport};
use desk_turn::model::TurnSettings;
use webrtc::api::APIBuilder;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::setting_engine::{SctpMaxMessageSize, SettingEngine};
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::{RTCPeerConnection, configuration::RTCConfiguration};

use crate::error::DeskError;
use crate::model::settings::{Settings, SystemSettings, TraversalMode};

/// The external `host:port` endpoints of the TURN server this node hosts
/// itself, used to recognise (and drop) a relay candidate that would point
/// back at our own bundled TURN.
///
/// Sourced from the live `TurnApiState` produced when the embedded TURN
/// server actually started (`None` when no embedded TURN is running — a
/// non-`Default`/`Signaling` startup, or a `startup_turn_server` failure),
/// so it stays in lock-step with the same `TurnApiState` the local signaling
/// uses to inject TURN. `None` yields an empty set: nothing is treated as
/// self-hosted, so no remote relay is ever dropped.
pub fn own_turn_endpoints(turn: Option<&TurnSettings>) -> HashSet<String> {
    turn.map(|t| {
        t.interfaces
            .iter()
            .map(|iface| iface.external.clone())
            .collect()
    })
    .unwrap_or_default()
}

/// Extract the `external` (`host:port`) token from a `turn:host:port?...` URL.
/// Only the `turn:` scheme is handled because [`LcxlRTCIceServer::transport`]
/// reports `Turn` solely for `turn:`-prefixed URLs (`turns:` never reaches the
/// TURN branch), so this is only ever called on `turn:` URLs.
fn turn_url_endpoint(url: &str) -> Option<&str> {
    url.strip_prefix("turn:")
        .map(|rest| rest.split('?').next().unwrap_or(rest))
}

/// Filter the request's ICE servers down to the ones this node should
/// actually use given the local `traversal_mode`.
///
/// `traversal_mode` is the operator's explicit traversal intent and decides
/// what kind of server is kept — independent of startup mode:
/// - `Turn` keeps both STUN and TURN.
/// - `Stun` keeps STUN, drops TURN.
/// - `None` drops everything (host candidates only).
///
/// On top of that, a TURN URL pointing back at this node's own bundled TURN
/// (`own_turn_endpoints`) is dropped at URL granularity: relaying through a
/// TURN server we host ourselves is pointless and, on a co-located portable
/// node, the self-allocation can stall ICE gathering long enough to starve
/// consent-freshness on the otherwise-working pair. A server keeps any of its
/// non-self URLs (and the credential that rides with them); it is removed
/// entirely only when every URL was self-hosted.
///
/// Servers with no / unrecognised transport are skipped with a warning.
/// Pure function — no I/O, no settings lookup, easy to unit test.
pub fn filter_ice_servers(
    request_ice_servers: &[LcxlRTCIceServer],
    traversal_mode: &TraversalMode,
    own_turn_endpoints: &HashSet<String>,
) -> Vec<LcxlRTCIceServer> {
    let mut filtered = Vec::new();
    for ice_server in request_ice_servers {
        match ice_server.transport() {
            Some(TurnTransport::Stun) => {
                if matches!(traversal_mode, TraversalMode::Stun | TraversalMode::Turn) {
                    filtered.push(ice_server.clone());
                }
            }
            Some(TurnTransport::Turn) => {
                if !matches!(traversal_mode, TraversalMode::Turn) {
                    continue;
                }
                if own_turn_endpoints.is_empty() {
                    filtered.push(ice_server.clone());
                    continue;
                }
                // Drop only the URLs that point back at our own TURN; keep the
                // rest of the object (URLs + shared credential) intact.
                let kept_urls: Vec<String> = ice_server
                    .urls
                    .iter()
                    .filter(|url| {
                        let is_self = turn_url_endpoint(url)
                            .is_some_and(|ep| own_turn_endpoints.contains(ep));
                        if is_self {
                            log::debug!("Dropping self-hosted TURN ICE url: {url}");
                        }
                        !is_self
                    })
                    .cloned()
                    .collect();
                if !kept_urls.is_empty() {
                    filtered.push(LcxlRTCIceServer {
                        urls: kept_urls,
                        username: ice_server.username.clone(),
                        credential: ice_server.credential.clone(),
                    });
                }
            }
            None => {
                log::warn!(
                    "Ignoring ICE server with invalid/empty transport: {:?}",
                    ice_server
                );
            }
        }
    }
    filtered
}

/// Built-in default for the ICE `disconnected` timeout, used when
/// `system.webrtc_ice_disconnected_timeout_secs` is `None`. Equals the
/// webrtc-rs library default — the daemon doesn't lean on this layer
/// for fast cleanup. The signaling-layer `ConnectionRemoved`
/// notification (delivered the moment a browser closes its WS) is the
/// primary path that triggers daemon-side `cleanup_pc`. ICE timeouts
/// here are the fallback for the case where signaling itself is gone
/// too — at which point we want to behave like a normal WebRTC peer
/// and absorb realistic network jitter.
pub const DEFAULT_DAEMON_ICE_DISCONNECTED_TIMEOUT_SECS: u64 = 5;

/// Built-in default for the ICE `failed` timeout, used when
/// `system.webrtc_ice_failed_timeout_secs` is `None`. Tightened from
/// the webrtc-rs default of 25 s to 15 s: combined budget of 20 s
/// (default disconnected + failed) caps how long the worker's DXGI
/// duplication stays alive after both signaling and ICE have gone
/// silent. The webrtc-rs default of 30 s was demonstrably long enough
/// for a user-driven reopen (3-4 s) to race the still-running capture
/// loop and crash the new pipeline with `0x80070057 (E_INVALIDARG)`
/// from a second `DuplicateOutput` call.
pub const DEFAULT_DAEMON_ICE_FAILED_TIMEOUT_SECS: u64 = 15;

/// Resolve the effective ICE timeouts from settings, falling back to
/// the built-in defaults above when the operator hasn't set explicit
/// overrides. Pulled out so `build_peer_connection` and the unit
/// tests share the same resolution path.
pub(super) fn resolve_ice_timeouts(system: &SystemSettings) -> (Duration, Duration) {
    let disconnected = Duration::from_secs(
        system
            .webrtc_ice_disconnected_timeout_secs
            .unwrap_or(DEFAULT_DAEMON_ICE_DISCONNECTED_TIMEOUT_SECS),
    );
    let failed = Duration::from_secs(
        system
            .webrtc_ice_failed_timeout_secs
            .unwrap_or(DEFAULT_DAEMON_ICE_FAILED_TIMEOUT_SECS),
    );
    (disconnected, failed)
}

/// Build an `RTCPeerConnection` with the lcxl-remote-desk daemon
/// defaults:
///
/// - 127.0.0.1 host candidate is included so loopback browser / Tauri
///   webview connections succeed via the local pair without requiring
///   cross-interface routing.
/// - SCTP `max_message_size_can_send` is set to `Unbounded` so large
///   DataChannel payloads (file-transfer, large clipboard) do not
///   fragment.
/// - ICE disconnected / failed timeouts come from
///   [`resolve_ice_timeouts`] — operator-tunable via settings, defaults
///   tighter than webrtc-rs so the cleanup fallback eventually fires
///   even when signaling is also gone. Active cleanup runs through
///   the signaling-side `ConnectionRemoved` hook and is unaffected by
///   these.
/// - Default codec set + default interceptor registry.
///
/// `ice_servers` is the already-filtered list (see
/// [`filter_ice_servers`]); pass `vec![]` for no ICE servers.
pub async fn build_peer_connection(
    ice_servers: Vec<RTCIceServer>,
    settings: &Settings,
) -> Result<RTCPeerConnection, DeskError> {
    let (ice_disconnected, ice_failed) = resolve_ice_timeouts(&settings.system);
    let mut setting_engine = SettingEngine::default();
    setting_engine.set_sctp_max_message_size_can_send(SctpMaxMessageSize::Unbounded);
    setting_engine.set_include_loopback_candidate(true);
    setting_engine.set_ice_timeouts(Some(ice_disconnected), Some(ice_failed), None);

    let mut m = MediaEngine::default();
    m.register_default_codecs()?;
    let mut registry = Registry::new();
    registry = register_default_interceptors(registry, &mut m)?;

    let api = APIBuilder::new()
        .with_setting_engine(setting_engine)
        .with_media_engine(m)
        .with_interceptor_registry(registry)
        .build();

    let config = RTCConfiguration {
        ice_servers,
        ..Default::default()
    };

    Ok(api.new_peer_connection(config).await?)
}
