//! Manager-link gating and remote-access central-source selection.

use super::*;

/// Resolve when the shared manager-link gate flips to disabled. For links the
/// manager toggle does not govern (`None` receiver) this never resolves, so the
/// `select!` branch that awaits it stays inert.
pub(super) async fn wait_manager_link_disabled(rx: &mut Option<watch::Receiver<bool>>) {
    match rx {
        Some(rx) => {
            // `wait_for` re-checks the current value, so a disable that already
            // happened is observed rather than missed.
            let _ = rx.wait_for(|enabled| !*enabled).await;
        }
        None => std::future::pending::<()>().await,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RemoteAccessCentralLink {
    None,
    Local,
    RemoteSignal,
    Manager,
}

pub(super) async fn remote_access_link_is_primary(
    settings: &web::Data<SharedSettings>,
    candidate: RemoteAccessCentralLink,
) -> bool {
    let settings = settings.read().await;
    let manager = manager_link_should_connect(
        &settings.system.manager_url,
        &settings.system.manager_api_token,
        settings.system.manager_enabled,
    );
    let remote_signal = settings
        .system
        .signaling_url
        .as_ref()
        .is_some_and(|value| !value.is_empty())
        && settings
            .system
            .signaling_token
            .as_ref()
            .is_some_and(|value| !value.is_empty());
    let primary =
        select_remote_access_central_link(manager, remote_signal, &settings.args.startup_mode);
    candidate == primary
}

pub(super) fn select_remote_access_central_link(
    manager: bool,
    remote_signal: bool,
    startup_mode: &StartupMode,
) -> RemoteAccessCentralLink {
    if manager {
        RemoteAccessCentralLink::Manager
    } else if remote_signal {
        RemoteAccessCentralLink::RemoteSignal
    } else if matches!(
        startup_mode,
        StartupMode::Default | StartupMode::ServiceDaemon
    ) {
        // Both modes run an embedded signal endpoint and maintain a loopback
        // Server connection to it. ServiceDaemon must elect that connection as
        // the durable mirror when no external central is configured; otherwise
        // a successful local LockAll remains pending forever.
        RemoteAccessCentralLink::Local
    } else {
        RemoteAccessCentralLink::None
    }
}

pub(super) async fn receive_remote_access_command(
    rx: &mut Option<broadcast::Receiver<String>>,
) -> Option<String> {
    let Some(rx) = rx else {
        return std::future::pending::<Option<String>>().await;
    };
    loop {
        match rx.recv().await {
            Ok(command) => return Some(command),
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                warn!("[remote-access] skipped {skipped} stale peer eviction commands");
            }
            Err(broadcast::error::RecvError::Closed) => return None,
        }
    }
}

/// Which upstream link an inbound signaling frame arrived on. This is the
/// daemon-side notion of "where did this frame come from", distinct from the
/// central-side `AuthContext` ("how did this connection authenticate"). Only the
/// `TrustedCentral` link is a trusted policy-decision upstream that may inject an
/// [`AuthorizedControlPayload`]; the local and remote-signaling links carry bare
/// payloads gated by local config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboundSignalingSource {
    /// The in-process / loopback signaling link (single-machine and the
    /// service-daemon's own API). No fleet PDP.
    Local,
    /// A bare remote signaling relay link (WebRTC signaling only). This link is
    /// NOT trusted as a central brain and must never be promoted to inject
    /// authorization — doing so would let any relay signaling server gain
    /// central-level injection rights.
    RemoteSignaling,
    /// The trusted central-brain link — the only authorization-injecting
    /// upstream. Covers both the enterprise manager and an OSS signal acting as
    /// the central brain; the edge classifies a link as trusted-central only
    /// from the connection's authentication result (the central credential
    /// slot), never from a bare relay.
    TrustedCentral,
}
