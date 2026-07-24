//! RequestRemote peer-connection admission and initialization.

use super::*;

/// Daemon side of `SignalingType::RequestRemote`. Creates the PC and
/// emits the matching `Init` reply. Mirrors the worker's
/// `init_ptc_peer_connection` minus the preapproved restoration (PC
/// lives in the daemon and never has to be rehydrated across worker
/// swaps) and minus the device-list enumeration (supplied instead by
/// the worker's `Capabilities` message).
#[allow(
    clippy::too_many_arguments,
    reason = "Daemon-side RequestRemote handler aggregates state from the \
              entire RouterContext; bundling into a struct would force a \
              tighter Arc/RwLock surface than the call sites need."
)]
pub async fn handle_request_remote(
    registry: &PcRegistry,
    outbound: &OutboundSink,
    settings: &Settings,
    user_name: &str,
    has_tauri: bool,
    capabilities: Option<&MediaCapabilities>,
    worker_mgr: Option<&WorkerManager>,
    virtual_display: Option<&Arc<crate::daemon::virtual_display::VirtualDisplaySupervisor>>,
    model: &SignalingModel,
    // The validated capability ceiling: unwrapped from the `RequestRemoteAuthz`
    // stamp for a redeemed grant (a temporary-support session is one such grant),
    // or `None` for an owner / unrestricted connection. Stored on the connection's
    // `SignalingState` and registered with the worker so the `meet(ceiling,
    // global)` gates enforce it.
    access_ceiling: Option<SecuritySettings>,
    // The grant logical-session id this connection belongs to (`None` when there
    // is no grant). Indexes the connection for grant-directed teardown.
    grant_session_id: Option<String>,
    // The device generation this grant was minted at (stamped by the central).
    // Recorded with the grant so a dial-code regeneration can direct-close every
    // session at a superseded generation. Ignored when there is no grant.
    grant_generation: i64,
) -> Result<(), DeskError> {
    let from_connection_id = model.check_and_get_from_connection_id()?;
    let request_remote = model.get_data::<RequestRemoteModel>()?;

    // Register the validated ceiling with the worker's per-connection ceiling map
    // ahead of any worker-bound frame for this connection, so the worker-side
    // `meet(ceiling, global)` gates enforce it from the first file-list / terminal
    // / media request (the never-drop event pipe keeps this FIFO-ordered before
    // them). Only grant-restricted connections carry a ceiling. Fail-closed: if the
    // registration cannot be delivered we abort the whole `RequestRemote` — done
    // *before* creating the PC so a rejected grant leaves no registered connection
    // — rather than let a capped grant session run with no worker-side cap (a
    // delivered media/terminal frame with no ceiling would fall back to global-only
    // gating and over-permit). Owner/unrestricted connections (`ceiling == None`)
    // skip this and leave the worker map empty.
    if let Some(ceiling) = access_ceiling.as_ref() {
        let mgr = worker_mgr.ok_or_else(|| {
            DeskError::CustomError(CustomDeskError::new(
                DeskErrorCode::SYSTEM_ERROR,
                &format!(
                    "cannot admit grant session {from_connection_id}: no worker to receive its capability ceiling"
                ),
            ))
        })?;
        mgr.send_to_worker(ServiceToWorker::SetConnectionCeiling(
            desk_ipc_protocol::message::SetConnectionCeilingPayload {
                connection_id: from_connection_id.to_string(),
                ceiling: Some(ceiling.clone()),
            },
        ))
        .await
        .map_err(|e| {
            DeskError::CustomError(CustomDeskError::new(
                DeskErrorCode::SYSTEM_ERROR,
                &format!(
                    "cannot admit grant session {from_connection_id}: ceiling registration failed to reach worker: {e}"
                ),
            ))
        })?;
    }

    let ctx = registry
        .create_for_request_remote(from_connection_id, &request_remote, settings)
        .await?;

    // Record the admission class for the router's first door, keyed by the
    // server-authoritative connection id. Kept for the whole signaling connection
    // (survives a later `CloseControl` PC teardown) so a capped connection can
    // never be reclassified as an unadmitted owner-plane sender.
    registry
        .record_admission(
            from_connection_id,
            match access_ceiling.as_ref() {
                Some(c) => Admission::Capped(c.clone()),
                None => Admission::OwnerFull,
            },
        )
        .await;

    // Stamp the capability ceiling and grant id onto the connection before the ICE
    // / DataChannel handlers below and before the Init reply, so the worker-side
    // `meet(ceiling, global)` gates and grant-directed teardown observe them from
    // the connection's very first frame.
    {
        let ctx_guard = ctx.read().await;
        let mut st = ctx_guard.signaling_state.write().await;
        st.purpose = request_remote.purpose;
        st.access_ceiling = access_ceiling;
        st.grant_session_id = grant_session_id.clone();
    }
    if let Some(gsid) = grant_session_id.as_deref() {
        // Index the connection under its grant so a directed revocation / teardown
        // can reach every connection that shares the grant in one sweep.
        registry
            .index_grant_connection(gsid, grant_generation, from_connection_id)
            .await;
    }

    // Forward locally-gathered ICE candidates back to the browser. Must
    // happen before the Offer arrives (and definitely before
    // `set_local_description` triggers gathering) so that no host / srflx
    // candidate is silently dropped during the handshake window.
    {
        let ctx_guard = ctx.read().await;
        register_local_ice_candidate_forwarder(
            Arc::clone(&ctx_guard.pc),
            outbound.clone(),
            from_connection_id.to_string(),
        );
    }

    // Install the daemon-side `on_data_channel` router on the
    // freshly-created PC. Done before the Offer arrives so any
    // DataChannel the browser opens during SDP setup has its handlers
    // attached on first onopen / onmessage. `worker_mgr` is `Option`
    // so unit-test paths that only exercise SDP / ICE handlers do not
    // have to construct a WorkerManager.
    if let Some(mgr) = worker_mgr {
        let ctx_guard = ctx.read().await;
        register_data_channel_router(
            Arc::clone(&ctx_guard.pc),
            from_connection_id.to_string(),
            Arc::clone(&ctx_guard.signaling_state),
            Arc::clone(&ctx_guard.cursor_data_channel),
            Arc::clone(&ctx_guard.clipboard_data_channel),
            Arc::clone(&ctx_guard.file_transfer_data_channel),
            mgr.clone(),
        );
        // Cleanup hook: when ICE detects the browser is gone (Failed) or
        // the PC is explicitly closed, drop the registry entry and tell
        // the worker to release its per-connection encoder + DXGI /
        // WASAPI capture. Without this the worker keeps DuplicateOutput
        // held and the next remote-desktop attempt hits 0x80070057 from
        // a second concurrent DuplicateOutput on the same monitor.
        register_peer_connection_state_cleanup(
            Arc::clone(&ctx_guard.pc),
            registry.clone(),
            mgr.clone(),
            virtual_display.cloned(),
            from_connection_id.to_string(),
        );
    }

    // Populate the Init reply from the worker's
    // `WorkerToService::Capabilities` snapshot when available; fall
    // back to capture-engine's static factory enumerations for the
    // codec lists when the worker hasn't reported yet (first-Init
    // race window). The fallback path leaves device lists empty
    // because device enumeration requires a live capture stack on
    // the worker's desktop — the daemon (running as SYSTEM in
    // ServiceDaemon mode) cannot produce a meaningful list itself.
    let (
        audio_encoder_list,
        video_encoder_list,
        audio_device_list,
        video_device_list,
        is_admin_value,
    ) = if let Some(caps) = capabilities {
        // Prefer the verbatim encoder identifiers reported by the worker
        // so the UI sees the X264 (libx264) vs H264 (OpenH264)
        // distinction; collapsing them through `media_codec_to_str` would
        // produce two indistinguishable "H264" entries. Fall back to the
        // codec-derived list only when the worker predates this field
        // (empty default on the wire).
        let video_encoder_list = if caps.video_encoders.is_empty() {
            caps.video_codecs
                .iter()
                .filter_map(media_codec_to_str)
                .collect::<Vec<_>>()
        } else {
            caps.video_encoders.clone()
        };
        let audio_encoder_list = if caps.audio_encoders.is_empty() {
            caps.audio_codecs
                .iter()
                .filter_map(media_codec_to_str)
                .collect::<Vec<_>>()
        } else {
            caps.audio_encoders.clone()
        };
        (
            audio_encoder_list,
            video_encoder_list,
            caps.audio_device_list.clone(),
            caps.video_device_list.clone(),
            caps.is_admin,
        )
    } else {
        (
            list_audio_encoder(),
            list_video_encoder(),
            std::collections::BTreeMap::new(),
            std::collections::BTreeMap::new(),
            desk_utils::permission::is_admin(),
        )
    };
    // Adaptive-resolution metadata. The browser hook uses
    // `virtual_display_active` to decide whether to start its
    // ResizeObserver loop at all, `virtual_display_device_name` to
    // confirm the captured monitor is in fact the IDD (otherwise
    // resizing the browser would silently change the virtual display
    // resolution while WGC keeps capturing a physical screen), and
    // `adaptive_resolution` to drive the trailing-edge debounce /
    // min-delta thresholds without needing a separate REST round-trip.
    // `virtual_display_current_refresh_hz` is informational — the auto
    // path always sends `refresh_hz: 0` and the daemon substitutes the
    // cached refresh on the way out.
    let (virtual_display_active, virtual_display_current_refresh_hz, virtual_display_device_name) =
        match virtual_display {
            Some(s) => (
                s.is_active().await,
                s.last_refresh_hz(),
                s.attached_display_name().await,
            ),
            None => (false, 0, None),
        };
    let adaptive_resolution = desk_signal_facade::model::signal::AdaptiveResolutionParams {
        debounce_ms: settings.virtual_display.adaptive_debounce_ms,
        min_delta_px: settings.virtual_display.adaptive_min_delta_px,
    };
    let init_data = InitSignalingData {
        ice_servers: vec![],
        user_name: user_name.to_string(),
        audio_device_list,
        audio_encoder_list,
        video_device_list,
        video_encoder_list,
        desk_settings: settings.desk.clone(),
        has_tauri,
        is_admin: is_admin_value,
        virtual_display_active,
        virtual_display_current_refresh_hz,
        virtual_display_device_name,
        adaptive_resolution,
        // The daemon/server process runs on the host, so the compile-time OS
        // is the host's OS. The browser uses this to tailor host-targeted UI.
        operation_system: desk_signal_facade::model::os::OperationSystemEnum::default(),
    };
    log::info!(
        "[pc_manager] Sending Init reply for {from_connection_id} \
         (capabilities={})",
        if capabilities.is_some() {
            "from-worker"
        } else {
            "fallback"
        }
    );
    send_response(
        outbound,
        &model.request_id,
        SignalingType::Init,
        from_connection_id,
        Some(&init_data),
    )
}
