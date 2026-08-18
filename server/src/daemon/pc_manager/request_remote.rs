//! RequestRemoteAccess peer-connection admission and initialization.

use super::*;

#[cfg(target_os = "linux")]
fn resolve_wayland_control_mode_for_admission(
    request_remote: &RequestRemoteModel,
    wayland: bool,
    portal_snapshot: Option<&desk_wayland_portal::PortalSnapshot>,
) -> Result<Option<LinuxInputControlMode>, CustomDeskError> {
    if request_remote.purpose
        != desk_signal_facade::model::signal::RemoteSessionPurpose::RemoteDesktop
    {
        return Ok(None);
    }

    let requested = request_remote
        .requested_wayland_control_mode
        .as_deref()
        .and_then(LinuxInputControlMode::parse)
        .ok_or_else(|| {
            CustomDeskError::new(
                DeskErrorCode::INVALID_PARAMS,
                "Linux remote desktop requires requested_wayland_control_mode=auto|none|uinput|portal",
            )
        })?;
    let resolved = requested.resolve(wayland);
    if wayland
        && !portal_snapshot.is_some_and(|snapshot| snapshot.admits(resolved.needs_portal_input()))
    {
        return Err(CustomDeskError::new(
            DeskErrorCode::WAYLAND_PORTAL_AUTHORIZATION_REQUIRED,
            "Wayland remote access must be enabled on the host before connecting",
        ));
    }
    Ok(Some(resolved))
}

/// Daemon side of `SignalingType::RequestRemoteAccess`. Creates the PC and
/// emits the matching `Init` reply. Mirrors the worker's
/// `init_ptc_peer_connection` minus the preapproved restoration (PC
/// lives in the daemon and never has to be rehydrated across worker
/// swaps) and minus the device-list enumeration (supplied instead by
/// the worker's `Capabilities` message).
#[allow(
    clippy::too_many_arguments,
    reason = "Daemon-side RequestRemoteAccess handler aggregates state from the \
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

    if request_remote.purpose
        == desk_signal_facade::model::signal::RemoteSessionPurpose::RemoteDesktop
        && worker_mgr.is_some_and(WorkerManager::media_worker_restart_required)
    {
        return Err(DeskError::CustomError(CustomDeskError::new(
            DeskErrorCode::MEDIA_WORKER_RESTART_REQUIRED,
            "the host media worker could not be stopped safely; restart the host application or service",
        )));
    }

    if request_remote.purpose
        == desk_signal_facade::model::signal::RemoteSessionPurpose::RemoteDesktop
        && capabilities.is_none()
    {
        return Err(DeskError::CustomError(CustomDeskError::new(
            DeskErrorCode::REMOTE_DESKTOP_CAPABILITIES_NOT_READY,
            "the current desktop worker has not published media capabilities yet",
        )));
    }

    #[cfg(target_os = "linux")]
    let resolved_wayland_control_mode = {
        let portal_snapshot = worker_mgr.and_then(WorkerManager::wayland_portal_snapshot);
        let wayland = worker_mgr
            .is_some_and(|manager| manager.linux_display_server() == LinuxDisplayServer::Wayland)
            || portal_snapshot.is_some()
            || capabilities
                .is_some_and(|caps| caps.video_device_list.contains_key("WAYLANDPORTAL"));
        resolve_wayland_control_mode_for_admission(
            &request_remote,
            wayland,
            portal_snapshot.as_ref(),
        )
        .map_err(DeskError::CustomError)?
    };
    #[cfg(not(target_os = "linux"))]
    let resolved_wayland_control_mode = None;

    // Register the validated ceiling with the worker's per-connection ceiling map
    // ahead of any worker-bound frame for this connection, so the worker-side
    // `meet(ceiling, global)` gates enforce it from the first file-list / terminal
    // / media request (the never-drop event pipe keeps this FIFO-ordered before
    // them). Only grant-restricted connections carry a ceiling. Fail-closed: if the
    // registration cannot be delivered we abort the whole `RequestRemoteAccess` — done
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
    // (survives a later `CloseRemoteSession` PC teardown) so a capped connection can
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
    // / DataChannel handlers below and before the RemoteAccessInitialized response, so the worker-side
    // `meet(ceiling, global)` gates and grant-directed teardown observe them from
    // the connection's very first frame.
    {
        let ctx_guard = ctx.read().await;
        let mut st = ctx_guard.signaling_state.write().await;
        st.purpose = request_remote.purpose;
        st.resolved_wayland_control_mode = resolved_wayland_control_mode;
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
            ctx_guard.connection_epoch.clone(),
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

    // Populate the RemoteAccessInitialized response from the worker's
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
        video_encoder_capabilities,
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
            caps.video_encoder_capabilities.clone(),
            caps.audio_device_list.clone(),
            caps.video_device_list.clone(),
            caps.is_admin,
        )
    } else {
        {
            let video_encoder_list = list_video_encoder();
            (
                list_audio_encoder(),
                video_encoder_list.clone(),
                desk_signal_facade::model::media_capability::capabilities_for_encoder_names(
                    &video_encoder_list,
                ),
                std::collections::BTreeMap::new(),
                std::collections::BTreeMap::new(),
                desk_utils::permission::is_admin(),
            )
        }
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
    let audio_capable = host_audio_capable(&audio_encoder_list, &audio_device_list);
    let connection_epoch = ctx.read().await.connection_epoch.clone();
    let init_data = RemoteAccessInitializedData {
        ice_servers: vec![],
        user_name: user_name.to_string(),
        audio_device_list,
        audio_encoder_list,
        video_device_list,
        video_encoder_list,
        video_encoder_capabilities,
        suggested_session_settings:
            desk_signal_facade::model::remote_session::SuggestedSessionSettings::from_host_settings(
                &settings.desk,
                audio_capable,
            ),
        session_settings_capabilities:
            desk_signal_facade::model::remote_session::SessionSettingsCapabilities::desktop(
                audio_capable,
            ),
        connection_epoch,
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
        "[pc_manager] Sending RemoteAccessInitialized response for {from_connection_id} \
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
        SignalingType::RemoteAccessInitialized,
        from_connection_id,
        Some(&init_data),
    )
}

/// Whether this host can offer an audio track at all.
///
/// The device map carries one key per compiled capture backend, so it stays
/// non-empty even on a machine without any sound hardware (Windows without an
/// audio device still reports `{ "WASAPI": [] }`). Keying off the map alone
/// would advertise `capture_audio` to the controller, which then cannot build
/// the capture/device/encoder triple the wire settings require.
fn host_audio_capable(
    audio_encoder_list: &[String],
    audio_device_list: &std::collections::BTreeMap<
        String,
        Vec<desk_signal_facade::model::audio_capture::AudioDevice>,
    >,
) -> bool {
    !audio_encoder_list.is_empty()
        && audio_device_list
            .values()
            .any(|devices| !devices.is_empty())
}

#[cfg(test)]
mod audio_capability_tests {
    use super::*;
    use desk_signal_facade::model::audio_capture::{AudioDataFlow, AudioDevice};
    use std::collections::BTreeMap;

    fn device() -> AudioDevice {
        AudioDevice {
            id: "device-1".to_string(),
            firendly_name: "Speakers".to_string(),
            data_flow: AudioDataFlow::Render,
            default: true,
        }
    }

    #[test]
    fn a_backend_without_devices_does_not_make_the_host_audio_capable() {
        // A Windows host with no sound hardware still reports its compiled
        // backend key with an empty device list.
        let mut devices = BTreeMap::new();
        devices.insert("WASAPI".to_string(), Vec::new());
        assert!(!host_audio_capable(&["Opus".to_string()], &devices));
    }

    #[test]
    fn one_backend_with_a_device_makes_the_host_audio_capable() {
        let mut devices = BTreeMap::new();
        devices.insert("SILENT".to_string(), Vec::new());
        devices.insert("WASAPI".to_string(), vec![device()]);
        assert!(host_audio_capable(&["Opus".to_string()], &devices));
    }

    #[test]
    fn devices_without_an_encoder_are_not_offerable() {
        let mut devices = BTreeMap::new();
        devices.insert("WASAPI".to_string(), vec![device()]);
        assert!(!host_audio_capable(&[], &devices));
        assert!(!host_audio_capable(&[], &BTreeMap::new()));
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use desk_wayland_portal::{
        AuthorizationTarget, PortalAvailability, PortalCapabilities, PortalPhase, PortalSnapshot,
    };

    fn request(mode: Option<&str>) -> RequestRemoteModel {
        RequestRemoteModel {
            purpose: desk_signal_facade::model::signal::RemoteSessionPurpose::RemoteDesktop,
            requested_wayland_control_mode: mode.map(str::to_string),
            ..RequestRemoteModel::default()
        }
    }

    fn ready_snapshot(input_ready: bool) -> PortalSnapshot {
        PortalSnapshot {
            phase: PortalPhase::Ready,
            capabilities: PortalCapabilities {
                screen_ready: true,
                input_ready,
            },
            availability: PortalAvailability::default(),
            target: Some(if input_ready {
                AuthorizationTarget::ScreenAndInput
            } else {
                AuthorizationTarget::ScreenOnly
            }),
            operation_id: None,
            generation: 1,
            restore_token_persisted: false,
            requires_local_action: false,
            reason_code: None,
            reason: None,
        }
    }

    #[test]
    fn screen_only_readiness_admits_none_and_uinput_but_not_portal() {
        let snapshot = ready_snapshot(false);
        for mode in ["none", "uinput"] {
            let resolved = resolve_wayland_control_mode_for_admission(
                &request(Some(mode)),
                true,
                Some(&snapshot),
            )
            .expect("screen-only mode should be admitted");
            assert_eq!(resolved.expect("resolved").as_str(), mode);
        }

        let error = resolve_wayland_control_mode_for_admission(
            &request(Some("portal")),
            true,
            Some(&snapshot),
        )
        .expect_err("Portal input requires input readiness");
        assert_eq!(
            error.error_code,
            DeskErrorCode::WAYLAND_PORTAL_AUTHORIZATION_REQUIRED
        );
    }

    #[test]
    fn auto_freezes_to_portal_on_wayland_and_requires_input_readiness() {
        let snapshot = ready_snapshot(true);
        assert_eq!(
            resolve_wayland_control_mode_for_admission(
                &request(Some("auto")),
                true,
                Some(&snapshot),
            )
            .expect("ready")
            .expect("resolved"),
            LinuxInputControlMode::Portal
        );
    }

    #[test]
    fn missing_mode_is_a_protocol_error_before_peer_creation() {
        let error = resolve_wayland_control_mode_for_admission(
            &request(None),
            true,
            Some(&ready_snapshot(true)),
        )
        .expect_err("mode is mandatory for Linux remote desktop");
        assert_eq!(error.error_code, DeskErrorCode::INVALID_PARAMS);
    }
}
