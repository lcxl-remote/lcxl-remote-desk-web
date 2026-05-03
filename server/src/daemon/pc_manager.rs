//! # Daemon-side WebRTC PeerConnection manager (Arch IV)
//!
//! Owner of the [`webrtc::peer_connection::RTCPeerConnection`] lifecycle.
//! In Arch III the worker process held the PC, which meant every UAC /
//! lock-screen / OS-session-switch (any event that respawns the worker)
//! tore down the PC and forced the browser through full SDP renegotiation
//! + ICE restart — a path that became unstable under SYSTEM-token +
//! Winlogon desktop combinations and showed up as "video garbled / ICE
//! checking → failed" during UAC.
//!
//! Arch IV moves the PC into the daemon: WebRTC negotiation happens once
//! per browser session and survives every worker swap. Worker replacement
//! becomes invisible to the browser apart from a ~1 s frame freeze waiting
//! for the next IDR from the new encoder.
//!
//! ## Status
//!
//! Cut 2 of PR 2: low-level PC construction helpers extracted from
//! `service::signaling::init_ptc_peer_connection`. Cut 3 wires the
//! daemon-side signaling router on top so the daemon directly drives
//! the SDP / ICE handshake.

use desk_signal_facade::model::signal::{LcxlRTCIceServer, TurnTransport};
use webrtc::api::APIBuilder;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::setting_engine::{SctpMaxMessageSize, SettingEngine};
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::{RTCPeerConnection, configuration::RTCConfiguration};

use crate::error::DeskError;
use crate::model::settings::{StartupMode, TraversalMode};

/// Filter the request's ICE servers down to the ones this node should
/// actually use given the local `traversal_mode` and `startup_mode`.
///
/// - `Stun` mode keeps STUN servers and drops TURN.
/// - `Turn` mode keeps STUN; keeps TURN only on `DeskServer` startups
///   (other modes do not traverse so they do not need TURN credentials).
/// - `Direct` mode drops everything.
///
/// Servers with no / unrecognised transport are skipped with a warning.
/// Pure function — no I/O, no settings lookup, easy to unit test.
pub fn filter_ice_servers(
    request_ice_servers: &[LcxlRTCIceServer],
    traversal_mode: TraversalMode,
    startup_mode: StartupMode,
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
                if traversal_mode == TraversalMode::Turn
                    && startup_mode == StartupMode::DeskServer
                {
                    filtered.push(ice_server.clone());
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

/// Build an `RTCPeerConnection` with the lcxl-remote-desk daemon
/// defaults:
///
/// - 127.0.0.1 host candidate is included so loopback browser / Tauri
///   webview connections succeed via the local pair without requiring
///   cross-interface routing.
/// - SCTP `max_message_size_can_send` is set to `Unbounded` so large
///   DataChannel payloads (file-transfer, large clipboard) do not
///   fragment.
/// - Default codec set + default interceptor registry.
///
/// `ice_servers` is the already-filtered list (see
/// [`filter_ice_servers`]); pass `vec![]` for no ICE servers.
pub async fn build_peer_connection(
    ice_servers: Vec<RTCIceServer>,
) -> Result<RTCPeerConnection, DeskError> {
    let mut setting_engine = SettingEngine::default();
    setting_engine.set_sctp_max_message_size_can_send(SctpMaxMessageSize::Unbounded);
    setting_engine.set_include_loopback_candidate(true);

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

#[cfg(test)]
mod tests {
    use super::*;

    fn ice(url: &str) -> LcxlRTCIceServer {
        LcxlRTCIceServer {
            urls: vec![url.to_string()],
            username: String::new(),
            credential: String::new(),
        }
    }

    #[test]
    fn filter_keeps_stun_only_in_stun_mode() {
        let request = vec![
            ice("stun:stun.l.google.com:19302"),
            ice("turn:turn.example.com:3478"),
        ];
        let kept = filter_ice_servers(&request, TraversalMode::Stun, StartupMode::DeskServer);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].urls[0], "stun:stun.l.google.com:19302");
    }

    #[test]
    fn filter_keeps_both_in_turn_mode_for_desk_server() {
        let request = vec![
            ice("stun:stun.l.google.com:19302"),
            ice("turn:turn.example.com:3478"),
        ];
        let kept = filter_ice_servers(&request, TraversalMode::Turn, StartupMode::DeskServer);
        assert_eq!(kept.len(), 2);
    }

    /// In Turn mode but on a non-DeskServer startup, TURN servers are
    /// dropped because non-DeskServer modes never traverse — they would
    /// burn TURN credentials for nothing and risk a credential leak via
    /// the browser-side ICE candidate dump.
    #[test]
    fn filter_drops_turn_in_turn_mode_when_not_desk_server() {
        let request = vec![
            ice("stun:stun.l.google.com:19302"),
            ice("turn:turn.example.com:3478"),
        ];
        let kept =
            filter_ice_servers(&request, TraversalMode::Turn, StartupMode::ServiceDaemon);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].urls[0], "stun:stun.l.google.com:19302");
    }

    /// `TraversalMode::None` means "no STUN, no TURN, host candidates
    /// only". The filter drops everything from the request.
    #[test]
    fn filter_drops_everything_in_none_mode() {
        let request = vec![
            ice("stun:stun.l.google.com:19302"),
            ice("turn:turn.example.com:3478"),
        ];
        let kept = filter_ice_servers(&request, TraversalMode::None, StartupMode::DeskServer);
        assert!(kept.is_empty());
    }

    /// Servers with no recognisable transport scheme are skipped (and
    /// the daemon logs a warning) rather than admitted as unknown.
    #[test]
    fn filter_drops_unrecognised_transport() {
        let request = vec![
            ice("https://not-a-stun-or-turn.example.com"),
            ice("stun:stun.l.google.com:19302"),
        ];
        let kept = filter_ice_servers(&request, TraversalMode::Stun, StartupMode::DeskServer);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].urls[0], "stun:stun.l.google.com:19302");
    }

    /// Sanity: the construction path itself works with an empty ICE
    /// list (the daemon ICE-only-host case for portable mode).
    #[tokio::test]
    async fn build_peer_connection_succeeds_with_no_ice_servers() {
        let pc = build_peer_connection(vec![]).await.expect("build pc");
        // Just confirm we got a usable handle back; tear down via Drop.
        assert_eq!(
            pc.connection_state(),
            webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState::New
        );
    }
}
