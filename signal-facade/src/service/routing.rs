//! Local and cross-instance signaling routing utilities.

use super::*;

/// Place a signaling frame onto the WebSocket of the peer `to_connection_id` if
/// it is held in `connection_map`, rewriting the frame's from/to fields.
///
/// Returns `Ok(true)` if delivered, `Ok(false)` if the target is not in this
/// instance's map. The target [`ConnectionState`] is cloned out and the map lock
/// dropped before the `send_to_peer` await, so the map is never held across I/O.
///
/// Shared by [`SignalingHandler::forward_to_peer`] (its local branch) and the
/// manager's internal relay landing, so cross-instance delivery reproduces
/// single-instance semantics exactly. The `request_callback_map` shortcut stays in
/// `forward_to_peer` (it concerns the *origin* connection awaiting a reply, which
/// is always co-located with that connection); the relay landing only places the
/// frame onto the target peer's socket.
pub async fn deliver_to_local_peer(
    connection_map: &SharedConnectionMap,
    from_connection_id: &str,
    to_connection_id: &str,
    model: &SignalingModel,
) -> Result<bool, DeskSignalFacadeError> {
    let target = {
        let map = connection_map.read().await;
        map.get(to_connection_id).cloned()
    };
    let Some(target) = target else {
        return Ok(false);
    };
    target.send_to_peer(from_connection_id, model).await?;
    Ok(true)
}

/// Decide the outcome for a frame whose target connection is not held locally: try
/// the cross-instance [`PeerFrameRelay`] (if configured), otherwise honor
/// `ignore_connection_not_found` or surface `SESSION_NOT_FOUND`.
///
/// Split out of [`SignalingHandler::forward_to_peer`] so the relay decision is
/// unit-testable without a full handler (which needs a live WS `Session`). With
/// no relay (the signal server) this is exactly the original single-instance
/// behavior: a local miss is a genuine "connection not found".
pub async fn relay_or_not_found(
    peer_relay: &Option<Arc<dyn PeerFrameRelay>>,
    to_connection_id: &str,
    from_connection_id: &str,
    model: &SignalingModel,
    ignore_connection_not_found: bool,
) -> Result<(), DeskSignalFacadeError> {
    if let Some(relay) = peer_relay {
        match relay
            .relay(to_connection_id, from_connection_id, model)
            .await?
        {
            RelayOutcome::Delivered => return Ok(()),
            // Held by no instance — fall through to the not-found handling.
            RelayOutcome::NotFound => {}
        }
    }

    if ignore_connection_not_found {
        // Benign by contract: the caller flagged this miss as expected. A frame
        // reaches here when the daemon fans a browser-bound copy out to *every*
        // upstream link (local signal + manager + remote signal), so a central
        // that does not hold the target connection is simply "not the owner". Keep
        // it at debug so a genuine SESSION_NOT_FOUND is not drowned in this noise.
        log::debug!(
            "Connection {} is not found to forward signaling, ignore it: {:?}",
            to_connection_id,
            model
        );
        return Ok(());
    }
    DeskSignalFacadeError::custom_error(
        DeskErrorCode::SESSION_NOT_FOUND,
        &format!(
            "Connection {} is not found to forward signaling: {:?}",
            to_connection_id, model
        ),
    )
}

/// What to do with an inbound frame that matched no pending request-callback on
/// this central. The daemon broadcasts every browser-bound frame to *all* of the
/// host's upstream links (see `signaling_proxy`), so a response can land on a
/// central that neither issued the originating request (its callback lives on
/// another upstream) nor holds the target connection — such a copy is simply "not
/// for this central" and must not surface as an error.
pub enum UnmatchedForward<'a> {
    /// An orphaned response with no deliverable target (a body-less list/manager
    /// response carries no `to_connection_id`). It can never be delivered here —
    /// drop it quietly.
    Drop,
    /// Attempt delivery to this target connection (local map, then relay).
    Deliver {
        to: &'a str,
        /// Treat a local + relay miss as benign rather than SESSION_NOT_FOUND.
        ignore_not_found: bool,
    },
    /// A *request* with no target connection — a genuine protocol error.
    MissingTarget,
}

/// Classify an unmatched frame (see [`UnmatchedForward`]). A response that cannot
/// be delivered on this central is always benign (it is a broadcast copy for
/// another upstream / instance, or its origin browser has gone): with no target it
/// is dropped, and with an unreachable target the miss is ignored. Only a request
/// still requires a target and surfaces a miss as an error.
pub fn classify_unmatched_forward(
    model: &SignalingModel,
    ignore_connection_not_found: bool,
) -> UnmatchedForward<'_> {
    match model.to_connection_id.as_deref() {
        Some(to) => UnmatchedForward::Deliver {
            to,
            ignore_not_found: ignore_connection_not_found || model.is_response(),
        },
        None if model.is_response() => UnmatchedForward::Drop,
        None => UnmatchedForward::MissingTarget,
    }
}

// ====== Shared helpers ======

pub fn parse_ip_from_peer_addr(addr: &str) -> Option<IpAddr> {
    if let Ok(sock) = addr.parse::<SocketAddr>() {
        return Some(sock.ip());
    }
    if let Ok(ip) = addr.parse::<IpAddr>() {
        return Some(ip);
    }
    None
}

pub fn rewrite_mdns_candidate_with_ip(
    signaling_model: &SignalingModel,
    fallback_ip: IpAddr,
) -> Option<SignalingModel> {
    let data = match signaling_model.get_raw_data() {
        Some(d) => d.clone(),
        None => return None,
    };
    let mut obj = match data.as_object() {
        Some(o) => o.clone(),
        None => return None,
    };

    let candidate_value = obj.get("candidate")?;
    let candidate_str = candidate_value.as_str()?;

    if !candidate_str.contains(".local") {
        return None;
    }

    let mut parts = candidate_str
        .split_whitespace()
        .map(|s| s.to_string())
        .collect::<Vec<_>>();
    if parts.len() < 6 {
        return None;
    }

    let host = parts[4].clone();
    if !host.ends_with(".local") {
        return None;
    }

    parts[4] = fallback_ip.to_string();
    let new_candidate = parts.join(" ");
    obj.insert(
        "candidate".to_string(),
        serde_json::Value::String(new_candidate.clone()),
    );

    log::info!(
        "Rewrote mDNS ICE candidate using signaling peer IP {}: {} -> {}",
        fallback_ip,
        host,
        new_candidate
    );

    Some(SignalingModel::new(
        &signaling_model.request_id,
        signaling_model.signaling_type,
        signaling_model.from_connection_id.clone(),
        signaling_model.to_connection_id.clone(),
        Some(serde_json::Value::Object(obj)),
        signaling_model.response_state.clone(),
    ))
}

/// Run a request/response signaling call against a connection held in the local
/// map, **without holding the map lock across the await**.
///
/// The connection state is cloned under a brief read lock, the guard is dropped,
/// and only then is `request_peer_with_callback` awaited — so a slow desk server
/// can never serialize the whole map or risk a lock held across a 30s round trip.
/// Shared by the terminal proxy controller (and the manager's owner-local
/// handling), so all of them get the same no-lock behavior.
///
/// Returns the peer's response model. A connection absent from the local map
/// yields `REMOTE_DESK_OFFLINE` (`not_found_msg`); a peer error_code in the
/// response is surfaced as that error.
pub async fn request_on_local_connection<T>(
    connection_map: &SharedConnectionMap,
    connection_id: &str,
    signaling_type: SignalingType,
    data: Option<&T>,
    not_found_msg: &str,
) -> Result<SignalingModel, DeskSignalFacadeError>
where
    T: ?Sized + serde::Serialize + Sync,
{
    let state = {
        let guard = connection_map.read().await;
        guard.get(connection_id).cloned()
    };
    let Some(state) = state else {
        return DeskSignalFacadeError::custom_error(
            DeskErrorCode::REMOTE_DESK_OFFLINE,
            not_found_msg,
        );
    };
    let response = state
        .request_peer_with_callback(signaling_type, data, None)
        .await?;
    if let Some(ref response_state) = response.response_state
        && response_state.error_code != 0
    {
        return DeskSignalFacadeError::custom_error(
            DeskErrorCode::from_wire(response_state.error_code),
            &response_state.message.clone().unwrap_or_default(),
        );
    }
    Ok(response)
}
