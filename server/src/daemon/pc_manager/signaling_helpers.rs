//! Shared signaling response and ICE-forwarding helpers.

use super::*;

// =====================================================================
// SignalingType handlers
// =====================================================================

/// Outbound Sender used to ship a serialised SignalingModel back to
/// the signaling server (and thence to the browser). Identical to
/// `signaling_proxy`'s `outbound_tx` — pulled out as a type alias so
/// the handler signatures stay readable.
pub type OutboundSink = broadcast::Sender<String>;

/// Push a successful response back to the signaling server. Errors
/// are logged but not returned because a proxy connection drop is
/// recovery-by-reconnect, not a per-handler failure.
pub(super) fn send_response<T: serde::Serialize + ?Sized>(
    outbound: &OutboundSink,
    request_id: &str,
    signaling_type: SignalingType,
    to_connection_id: &str,
    data: Option<&T>,
) -> Result<(), DeskError> {
    let model = SignalingModel::success_response(
        request_id,
        signaling_type,
        None,
        Some(to_connection_id.to_string()),
        data,
    )?;
    let text = serde_json::to_string(&model).map_err(|e| {
        DeskError::CustomError(CustomDeskError::new(
            DeskErrorCode::SYSTEM_ERROR,
            &format!("Failed to encode signaling reply: {e}"),
        ))
    })?;
    if let Err(e) = outbound.send(text) {
        log::warn!("[pc_manager] outbound channel send failed: {e}");
    }
    Ok(())
}

/// Forward locally-gathered ICE candidates back to the browser via the
/// signaling channel. Each host / srflx / relay candidate emitted by
/// libwebrtc is wrapped in a
/// `SignalingType::IceCandidate` message — without this the browser only ever
/// learns about the daemon's transport addresses through peer-reflexive
/// discovery, which only works for single-m-line PCs (DataChannel-only
/// file transfer) and consistently times out for video+audio+DC PCs in
/// 30 s of `checking`. Trickle ICE friendly: each candidate ships
/// independently as a fresh `new_request`.
pub(super) fn register_local_ice_candidate_forwarder(
    pc: Arc<RTCPeerConnection>,
    outbound: OutboundSink,
    from_connection_id: String,
    connection_epoch: String,
) {
    pc.on_ice_candidate(Box::new(move |c: Option<RTCIceCandidate>| {
        let outbound = outbound.clone();
        let from_connection_id = from_connection_id.clone();
        let connection_epoch = connection_epoch.clone();
        Box::pin(async move {
            // None signals end-of-candidates; nothing to ship in that case.
            let Some(candidate) = c else {
                log::debug!(
                    "[pc_manager] ICE gathering complete for {from_connection_id} \
                     (end-of-candidates)"
                );
                return;
            };
            let init = match candidate.to_json() {
                Ok(j) => j,
                Err(e) => {
                    log::warn!(
                        "[pc_manager] candidate.to_json failed for {from_connection_id}: {e}"
                    );
                    return;
                }
            };
            let payload = desk_signal_facade::model::remote_session::IceCandidatePayload {
                connection_epoch,
                candidate: init.clone(),
            };
            let model = match SignalingModel::new_request(
                SignalingType::IceCandidate,
                Some(from_connection_id.clone()),
                Some(&payload),
            ) {
                Ok(m) => m,
                Err(e) => {
                    log::warn!(
                        "[pc_manager] candidate model build failed for {from_connection_id}: {e}"
                    );
                    return;
                }
            };
            match serde_json::to_string(&model) {
                Ok(text) => {
                    log::info!(
                        "[pc_manager] forwarding local ICE candidate for {from_connection_id}: \
                         {}",
                        init.candidate
                    );
                    if let Err(e) = outbound.send(text) {
                        log::warn!(
                            "[pc_manager] outbound send (IceCandidate) failed for \
                             {from_connection_id}: {e}"
                        );
                    }
                }
                Err(e) => {
                    log::warn!(
                        "[pc_manager] candidate JSON encode failed for {from_connection_id}: {e}"
                    );
                }
            }
        })
    }));
}
