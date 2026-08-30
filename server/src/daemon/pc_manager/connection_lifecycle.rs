//! Peer-connection teardown and grant-session cleanup.

use super::*;

/// Centralised teardown for one browser-side PC. Removes the registry
/// entry (so subsequent ICE / DC events for that connection short-circuit),
/// closes the underlying [`RTCPeerConnection`] (idempotent — safe even if
/// already closed by webrtc-rs internals), and ships `StopMedia` to the
/// worker so its per-connection encoder + DXGI duplication / WASAPI capture
/// release immediately. Used by:
///
/// 1. [`handle_close_remote_session`] — explicit browser session teardown.
/// 2. The on_peer_connection_state_change hook installed in
///    [`register_peer_connection_state_cleanup`] — fires when ICE
///    detects the browser is gone (Failed/Closed/Disconnected).
///
/// All errors swallowed: a dead worker / already-closed PC are normal
/// teardown paths, not failure modes the caller can recover from.
pub async fn cleanup_pc(
    registry: &PcRegistry,
    worker_mgr: &WorkerManager,
    virtual_display: Option<&Arc<crate::daemon::virtual_display::VirtualDisplaySupervisor>>,
    connection_id: &str,
    reason: &str,
) {
    let removed = registry.remove(connection_id).await;
    if removed.is_some()
        && let Some(activity) = registry.host_activity()
    {
        // A PC is an implementation detail inside the browser's longer-lived
        // signaling session. Preserve the desktop-view entry so a negotiated
        // codec replacement does not destroy/recreate the host indicator, while
        // clearing activity flags whose media/control path really stopped.
        activity.begin_pc_handoff(connection_id);
    }
    // Deny anything this connection was still waiting on an answer for. Without
    // this the dialog outlives the controller that raised it, and a user who
    // answers it afterwards approves work for a connection that is already gone
    // — an approval that would then be cached and honored for whatever
    // reconnects.
    if let Some(hub) = registry.host_control_hub() {
        let cancelled = hub.cancel_pending_for_connection(connection_id);
        if !cancelled.is_empty() {
            log::info!(
                "[pc_manager] cancelled {} pending approval(s) for {connection_id} (reason: {reason})",
                cancelled.len()
            );
        }
    }
    // Grant reverse-index ownership follows the signaling session, not this PC.
    // Keeping it through a replacement closes the revocation race where a grant
    // is revoked after the old PC closes but before the new PC is registered.
    if let Some(ctx) = &removed {
        let ctx = ctx.read().await;
        ctx.media_paused.store(true, Ordering::Relaxed);
        ctx.media_output_fence.write().await.audio_open = false;
        let mut coordinator = ctx.media_coordinator.lock().await;
        coordinator.closed = true;
        coordinator.current_apply_request_id = None;
        coordinator.pending_audio_approval_id = None;
        coordinator.pending_audio_candidate = None;
        coordinator.video_terminal_waiter = None;
        coordinator.audio_terminal_waiter = None;
        coordinator.audio_expected_terminal = None;
        coordinator.audio_desired_active = false;
        drop(coordinator);
        if let Err(e) = ctx.pc.close().await {
            log::warn!("[pc_manager] PC close failed for {connection_id}: {e}");
        }
        log::info!("[pc_manager] Closed PC for {connection_id} (reason: {reason})");
    } else {
        log::debug!("[pc_manager] cleanup_pc({connection_id}, {reason}): registry already empty");
    }

    if let Some(ctx) = &removed {
        let connection_epoch = ctx.read().await.connection_epoch.clone();
        if let Err(e) = worker_mgr
            .send_to_interactive_connection_worker(
                connection_id,
                ServiceToWorker::StopMedia(desk_ipc_protocol::message::StopMediaPayload {
                    connection_id: connection_id.to_string(),
                    connection_epoch,
                }),
            )
            .await
        {
            log::debug!("[pc_manager] StopMedia for {connection_id} could not reach worker: {e}");
        }
    }

    // Terminal WS connections hold no PC and no media, so the steps above are a
    // no-op for them. A directed teardown (grant revoke / dial-code regeneration)
    // sweeping this path must still physically end the terminal: kill the worker
    // shell and clear the connection's ceiling + admission so nothing survives the
    // revocation. Idempotent with the terminal's own `CloseTerminal` cleanup.
    if registry.is_terminal_connection(connection_id).await {
        if let Err(e) = worker_mgr
            .send_to_connection_worker(
                connection_id,
                ServiceToWorker::CloseTerminal(desk_ipc_protocol::message::CloseTerminalPayload {
                    connection_id: connection_id.to_string(),
                }),
            )
            .await
        {
            log::debug!(
                "[pc_manager] CloseTerminal for {connection_id} could not reach worker: {e}"
            );
        }
        if let Err(e) = worker_mgr
            .send_to_connection_worker(
                connection_id,
                ServiceToWorker::SetConnectionCeiling(
                    desk_ipc_protocol::message::SetConnectionCeilingPayload {
                        connection_id: connection_id.to_string(),
                        ceiling: None,
                    },
                ),
            )
            .await
        {
            log::debug!(
                "[pc_manager] ceiling clear for terminal {connection_id} could not reach worker: {e}"
            );
        }
        registry.clear_admission(connection_id).await;
        registry.unmark_terminal_connection(connection_id).await;
        log::info!("[pc_manager] terminal connection {connection_id} torn down (reason: {reason})");
    }

    // Re-derive the exclusive-mode desired flag on every actual
    // removal, not just the N → 0 case. If the
    // departing PC was the sole `accept_control=true` holder but
    // other view-only PCs remain (registry.len() stays > 0), the
    // old code never recomputed and the supervisor stayed pinned
    // at `desired=true` with no control holder → physical displays
    // left detached. `recompute_desired` is a no-op when no router
    // closure is installed (e.g. tests / in-process mode), and
    // costs only a read lock + one closure call otherwise, so it is
    // safe to run unconditionally on `removed.is_some()`.
    if let Some(supervisor) = virtual_display
        && removed.is_some()
    {
        supervisor.recompute_desired().await;
    }

    // N -> 0 virtual display detach. Four gates, all required:
    //   (1) `removed.is_some()` — only the call that actually pulled
    //       a live PC out triggers detach. Stale `ConnectionRemoved`
    //       fan-outs that arrive after the PC was already cleaned up
    //       (or never existed) MUST NOT trigger a detach, since a
    //       new `RequestRemoteAccess` may be mid-`ensure_attached` with no
    //       PC registered yet.
    //   (2) `registry.len() == 0` — no other live browser session
    //       still using the IDD.
    //   (3) `registry.pending_requests() == 0` — no other browser
    //       currently inside the `RequestRemoteAccess` handler holding a
    //       `PendingRequestGuard`. Without this gate, a fast browser
    //       open/close racing with a slow new connection's
    //       `ensure_attached` would tear down the IDD while the
    //       new connection is still bringing it up.
    //   (4) no admission remains for this signaling connection. An admission
    //       outlives its PC and therefore represents a logical browser session
    //       that may be replacing the PC after a wire-codec change.
    if let Some(supervisor) = virtual_display
        && removed.is_some()
        && registry.len().await == 0
        && registry.pending_requests() == 0
        && registry.admission(connection_id).await.is_none()
    {
        log::info!("[pc_manager] last PC removed, no pending requests; detaching virtual display");
        if let Err(e) = supervisor.apply(false).await {
            log::warn!("[pc_manager] N->0 virtual display detach failed: {e}");
        }
    }
}

pub async fn hide_private_screen_best_effort(
    worker_mgr: &WorkerManager,
    connection_id: &str,
    reason: &str,
) {
    if let Err(error) = worker_mgr
        .send_to_connection_worker(
            connection_id,
            ServiceToWorker::SetPrivateScreenVisibility(
                desk_ipc_protocol::message::SetPrivateScreenVisibilityPayload {
                    request_id: uuid::Uuid::new_v4().to_string(),
                    connection_id: connection_id.to_string(),
                    visible: false,
                },
            ),
        )
        .await
    {
        log::debug!(
            "[pc_manager] private-screen lifecycle hide for {connection_id} \
             ({reason}) could not reach worker: {error}"
        );
    }
}

async fn detach_virtual_display_if_unused(
    registry: &PcRegistry,
    virtual_display: Option<&Arc<crate::daemon::virtual_display::VirtualDisplaySupervisor>>,
    reason: &str,
) {
    if let Some(supervisor) = virtual_display
        && registry.len().await == 0
        && registry.pending_requests() == 0
    {
        log::info!("[pc_manager] logical connection ended ({reason}); detaching virtual display");
        if let Err(error) = supervisor.apply(false).await {
            log::warn!("[pc_manager] logical teardown virtual display detach failed: {error}");
        }
    }
}

async fn finalize_logical_connection(
    registry: &PcRegistry,
    worker_mgr: &WorkerManager,
    virtual_display: Option<&Arc<crate::daemon::virtual_display::VirtualDisplaySupervisor>>,
    connection_id: &str,
    reason: &str,
) {
    hide_private_screen_best_effort(worker_mgr, connection_id, reason).await;
    if let Some(activity) = registry.host_activity() {
        activity.remove_connection(connection_id);
    }
    registry.clear_admission(connection_id).await;
    registry.unindex_grant_connection(connection_id).await;
    registry.unmark_terminal_connection(connection_id).await;
    detach_virtual_display_if_unused(registry, virtual_display, reason).await;
    worker_mgr.clear_connection_target(connection_id);
}

/// Host-initiated teardown has stronger semantics than browser `CloseRemoteSession`:
/// it tombstones the signaling id and clears the whole admission footprint.
pub async fn force_disconnect_connection(
    registry: &PcRegistry,
    worker_mgr: &WorkerManager,
    virtual_display: Option<&Arc<crate::daemon::virtual_display::VirtualDisplaySupervisor>>,
    connection_id: &str,
    reason: &str,
) -> bool {
    let existed = registry
        .all_connection_ids()
        .await
        .iter()
        .any(|candidate| candidate == connection_id);
    registry.tombstone_connection(connection_id).await;
    cleanup_pc(registry, worker_mgr, virtual_display, connection_id, reason).await;
    if existed {
        // Hide before clearing the worker ceiling. `finalize_logical_connection`
        // repeats this best-effort operation for its shared cleanup path.
        hide_private_screen_best_effort(worker_mgr, connection_id, reason).await;
    }
    if let Err(error) = worker_mgr
        .send_to_connection_worker(
            connection_id,
            ServiceToWorker::SetConnectionCeiling(
                desk_ipc_protocol::message::SetConnectionCeilingPayload {
                    connection_id: connection_id.to_string(),
                    ceiling: None,
                },
            ),
        )
        .await
    {
        log::debug!(
            "[pc_manager] force-disconnect ceiling clear for {connection_id} could not reach worker: {error}"
        );
    }
    if existed {
        finalize_logical_connection(registry, worker_mgr, virtual_display, connection_id, reason)
            .await;
    } else {
        worker_mgr.clear_connection_target(connection_id);
    }
    existed
}

/// Tear down every connection admitted under grant `grant_session_id` — a
/// grant-directed revocation. Called when a grant is revoked or its logical
/// session ends (e.g. the manager broadcasts a directed teardown after a device
/// dial-code regeneration), so every connection sharing the grant ends
/// physically, not just at the signaling layer. Snapshots the grant's connection
/// ids first (each `cleanup_pc` prunes the reverse-index), then closes each PC. A
/// no-op when the grant has no live connection.
pub async fn close_grant_session(
    registry: &PcRegistry,
    worker_mgr: &WorkerManager,
    virtual_display: Option<&Arc<crate::daemon::virtual_display::VirtualDisplaySupervisor>>,
    grant_session_id: &str,
    reason: &str,
) {
    let ids = registry.connections_for_grant(grant_session_id).await;
    for id in ids {
        cleanup_pc(registry, worker_mgr, virtual_display, &id, reason).await;
        finalize_logical_connection(registry, worker_mgr, virtual_display, &id, reason).await;
    }
}

/// Tear down every grant session whose recorded generation is at or below
/// `revoked_generation` — the directed teardown the manager triggers after a device
/// dial-code regeneration (each superseded grant is closed via
/// [`close_grant_session`], so all of its connections end together). Owner sessions
/// carry no grant and are never indexed, so they are untouched. A no-op when no
/// held grant is at or below the revoked generation.
///
/// Matches on generation alone, not device: this daemon serves a single device (one
/// desk-server = one `client_id`), so every grant it holds targets that one device
/// and the `RevokeAccessGrant` frame is delivered only to this host. If a daemon ever
/// hosted grants for more than one target device, this would need the frame's
/// `target_device` as a second filter dimension (stored per grant) to avoid closing
/// an unrelated device's grant that happens to share a generation number.
pub async fn close_grants_up_to_generation(
    registry: &PcRegistry,
    worker_mgr: &WorkerManager,
    virtual_display: Option<&Arc<crate::daemon::virtual_display::VirtualDisplaySupervisor>>,
    revoked_generation: i64,
    reason: &str,
) {
    let gsids = registry.grants_up_to_generation(revoked_generation).await;
    for gsid in gsids {
        close_grant_session(registry, worker_mgr, virtual_display, &gsid, reason).await;
    }
}

/// Wire the daemon-side cleanup path onto `pc.on_peer_connection_state_change`
/// so a browser disconnect / network drop / explicit close releases the
/// worker's encoder + capture resources promptly.
///
/// Without this hook the worker keeps the per-connection encoder running and
/// the per-output DXGI duplication held; the next browser to connect then
/// hits `DuplicateOutput → 0x80070057 (E_INVALIDARG)` because Windows only
/// allows one duplication per (process, output) pair. Replaces the
/// `peer_state_change_sender → DeskSessionMessage::WebRTCDropped` chain
/// that used to live in `service::signaling::DeskSession::init_ptc_peer_connection`.
///
/// Only `Failed` and `Closed` trigger cleanup. `Disconnected` is transient
/// (a momentary network blip can recover) and webrtc-rs will follow it
/// with `Failed` after its internal disconnected-timeout if the peer
/// stays gone, so reacting to `Disconnected` would tear down working
/// connections during normal jitter.
pub(super) fn register_peer_connection_state_cleanup(
    pc: Arc<RTCPeerConnection>,
    registry: PcRegistry,
    worker_mgr: WorkerManager,
    virtual_display: Option<Arc<crate::daemon::virtual_display::VirtualDisplaySupervisor>>,
    connection_id: String,
) {
    // A real ICE restart can temporarily make the RTP path unavailable while
    // the worker keeps encoding. The first decodable frame after connectivity
    // returns may therefore be a P-frame whose reference IDR was lost. Track
    // whether this PC has connected before so every *subsequent* Connected
    // transition requests a fresh keyframe; the initial connection continues
    // to rely on StartMedia's normal first-IDR path.
    let has_connected_once = Arc::new(AtomicBool::new(false));
    pc.on_peer_connection_state_change(Box::new(move |state: RTCPeerConnectionState| {
        let registry = registry.clone();
        let worker_mgr = worker_mgr.clone();
        let virtual_display = virtual_display.clone();
        let connection_id = connection_id.clone();
        let has_connected_once = Arc::clone(&has_connected_once);
        Box::pin(async move {
            match state {
                RTCPeerConnectionState::Connected => {
                    if let Some(activity) = registry.host_activity() {
                        activity.set_pc_connected(&connection_id, true);
                    }
                    if mark_connected_and_should_force_keyframe(&has_connected_once) {
                        log::info!(
                            "[pc_manager] PC for {connection_id} reconnected; requesting a fresh keyframe"
                        );
                        if let Err(e) = worker_mgr
                            .send_to_interactive_connection_worker(&connection_id, ServiceToWorker::ForceKeyframe(
                                ForceKeyframePayload {
                                    connection_id: connection_id.clone(),
                                },
                            ))
                            .await
                        {
                            log::debug!(
                                "[pc_manager] reconnect ForceKeyframe for {connection_id} could not reach worker: {e}"
                            );
                        }
                    }
                }
                RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed => {
                    log::info!(
                        "[pc_manager] PC for {connection_id} reached terminal state {state:?}; \
                         tearing down daemon-side context + StopMedia to worker"
                    );
                    cleanup_pc(
                        &registry,
                        &worker_mgr,
                        virtual_display.as_ref(),
                        &connection_id,
                        "pc_state_terminal",
                    )
                    .await;
                }
                _ => {}
            }
        })
    }));
}

fn mark_connected_and_should_force_keyframe(has_connected_once: &AtomicBool) -> bool {
    has_connected_once.swap(true, Ordering::AcqRel)
}

/// Daemon side of `SignalingType::CloseRemoteSession`. Removes the
/// per-connection context, closes the PC, and tells the worker to
/// drop its per-`connection_id` encoder via
/// `ServiceToWorker::StopMedia`. The StopMedia is best-effort — a
/// dead worker will surface an error from `send_to_worker` which we
/// log but don't propagate; the PC is already closed at that point
/// so the daemon-side state is consistent regardless.
pub async fn handle_close_remote_session(
    registry: &PcRegistry,
    worker_mgr: &WorkerManager,
    virtual_display: Option<&Arc<crate::daemon::virtual_display::VirtualDisplaySupervisor>>,
    model: &SignalingModel,
) -> Result<(), DeskError> {
    let from_connection_id = model.check_and_get_from_connection_id()?;
    let payload =
        model.get_data::<desk_signal_facade::model::remote_session::CloseRemoteSessionPayload>()?;
    if let Some(context) = registry.get(from_connection_id).await {
        if context.read().await.connection_epoch != payload.connection_epoch {
            return Ok(());
        }
        cleanup_pc(
            registry,
            worker_mgr,
            virtual_display,
            from_connection_id,
            "close_remote_session",
        )
        .await;
    }
    if payload.finalize_logical_connection {
        finalize_logical_connection(
            registry,
            worker_mgr,
            virtual_display,
            from_connection_id,
            "close_remote_session_finalized",
        )
        .await;
    }
    Ok(())
}

/// Daemon side of `SignalingType::ConnectionRemoved`. Sent by the
/// signaling server when a `Browser`-type peer leaves its connection
/// map (typically because the browser tab closed and the WS
/// disconnected). The signal arrives milliseconds after the browser
/// goes away, well before webrtc-rs would notice through ICE consent
/// freshness — so this is the primary cleanup path for the
/// "user closed the tab" case. The matching ICE
/// `disconnected → failed` timeouts (see [`build_peer_connection`]
/// callers) only run when the signaling channel is gone too.
///
/// Idempotent: if no PC exists for `from_connection_id` the call is
/// a logged no-op (e.g. the browser never finished SDP, or another
/// cleanup path already fired). The departed peer's id rides in
/// `from_connection_id`; the data payload is intentionally empty.
pub async fn handle_connection_removed(
    registry: &PcRegistry,
    worker_mgr: &WorkerManager,
    virtual_display: Option<&Arc<crate::daemon::virtual_display::VirtualDisplaySupervisor>>,
    model: &SignalingModel,
) -> Result<(), DeskError> {
    let from_connection_id = model.check_and_get_from_connection_id()?;
    let known_logical_connection = registry
        .all_connection_ids()
        .await
        .iter()
        .any(|connection_id| connection_id == from_connection_id);
    cleanup_pc(
        registry,
        worker_mgr,
        virtual_display,
        from_connection_id,
        "peer_signaling_closed",
    )
    .await;
    // The signaling connection is truly ending (not just a `CloseRemoteSession`
    // PC teardown), so release every logical-session-owned resource.
    if known_logical_connection {
        finalize_logical_connection(
            registry,
            worker_mgr,
            virtual_display,
            from_connection_id,
            "peer_signaling_closed",
        )
        .await;
    }
    Ok(())
}

#[cfg(test)]
mod reconnect_tests {
    use super::*;

    #[test]
    fn only_a_reconnected_peer_requires_an_extra_keyframe() {
        let has_connected_once = AtomicBool::new(false);

        assert!(!mark_connected_and_should_force_keyframe(
            &has_connected_once
        ));
        assert!(mark_connected_and_should_force_keyframe(
            &has_connected_once
        ));
    }
}
