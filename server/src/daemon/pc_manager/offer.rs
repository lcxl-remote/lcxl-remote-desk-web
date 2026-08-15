//! SDP offer handling and media negotiation.

use super::*;

/// Inverse of the worker-side codec mapping. Used by the RemoteAccessInitialized response
/// path so the daemon's `audio_encoder_list` / `video_encoder_list`
/// payloads carry the same string identifiers the legacy worker did.
pub(super) fn media_codec_to_str(c: &MediaCodec) -> Option<String> {
    match c {
        MediaCodec::H264 => Some("H264".to_string()),
        MediaCodec::Vp8 => Some("VP8".to_string()),
        MediaCodec::Vp9 => Some("VP9".to_string()),
        MediaCodec::Av1 => Some("AV1".to_string()),
        MediaCodec::Opus => Some("Opus".to_string()),
    }
}

/// Map the offer's `desk_settings.video_encoder` string to the IPC
/// `MediaCodec`. Used by `handle_offer` to compose `StartMediaPayload`.
/// Map the browser-supplied `DeskSettings.video_device_name` to the
/// `StartMediaPayload.video_device` Option carried over IPC. Empty
/// string means "no display selected yet" — the daemon passes `None`
/// so the worker's `payload_overrides` leaves its base
/// `video_device_name` untouched (which the capture-engine then
/// hard-errors on; never falls back to a default monitor). Any
/// non-empty value is propagated verbatim.
pub(crate) fn video_device_for_payload(video_device_name: &str) -> Option<String> {
    if video_device_name.is_empty() {
        None
    } else {
        Some(video_device_name.to_string())
    }
}

pub(super) fn video_encoder_to_media_codec(t: VideoEncoderType) -> MediaCodec {
    match t {
        VideoEncoderType::H264 | VideoEncoderType::X264 => MediaCodec::H264,
        VideoEncoderType::VP8 => MediaCodec::Vp8,
        VideoEncoderType::VP9 => MediaCodec::Vp9,
        VideoEncoderType::AV1 => MediaCodec::Av1,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct NegotiatedVideo {
    pub codec: MediaCodec,
    pub encoder: VideoEncoderId,
}

pub(super) fn renegotiation_requires_full_reconnect(
    has_video_track: bool,
    previous_codec: Option<MediaCodec>,
    next_codec: Option<MediaCodec>,
) -> bool {
    has_video_track
        && matches!((previous_codec, next_codec), (Some(previous), Some(next)) if previous != next)
}

pub(super) fn should_restart_negotiated_pipeline(
    is_first_offer: bool,
    pipeline_was_retryable: bool,
    has_video: bool,
    negotiated_video: Option<NegotiatedVideo>,
) -> bool {
    !is_first_offer && pipeline_was_retryable && has_video && negotiated_video.is_some()
}

pub(super) fn initial_worker_media_flags(
    cached_start_video: bool,
    cached_start_audio: bool,
    video_blocked: bool,
) -> (bool, bool) {
    (cached_start_video && !video_blocked, cached_start_audio)
}

pub(super) fn is_video_offer_blocking_error(code: DeskErrorCode) -> bool {
    matches!(
        code,
        DeskErrorCode::FEATURE_UNAVAILABLE | DeskErrorCode::VIDEO_ENCODER_DIMENSIONS_UNSUPPORTED
    )
}

pub(super) fn blocked_offer_state(
    encoder: Option<VideoEncoderId>,
    source_resolution: Option<Resolution>,
    compatible_encoders: Vec<VideoEncoderId>,
    error: Option<&CustomDeskError>,
) -> MediaPipelineStateData {
    let reason_code = error
        .map(|error| error.error_code)
        .unwrap_or(DeskErrorCode::VIDEO_ENCODER_DIMENSIONS_UNSUPPORTED);
    let message = error.map_or_else(
        || match source_resolution {
            Some(source) => format!(
                "No installed encoder with known support can encode {}x{} for this controller",
                source.width, source.height
            ),
            None => "No installed encoder with known support is available for this controller"
                .to_string(),
        },
        |error| error.message.clone(),
    );
    MediaPipelineStateData {
        phase: MediaPipelinePhase::Blocked,
        encoder,
        source_resolution,
        compatible_encoders,
        reason_code: Some(reason_code),
        message: Some(message),
    }
}

pub(super) fn selected_capture_resolution(
    settings: &desk_signal_facade::model::desk_settings::DeskSettings,
    capabilities: &MediaCapabilities,
) -> Option<Resolution> {
    let displays = settings
        .image_capture
        .as_ref()
        .and_then(|backend| capabilities.video_device_list.get(backend))
        .or_else(|| capabilities.video_device_list.values().next())?;
    let display = if settings.video_device_name.is_empty() {
        displays.iter().find(|display| display.attached_to_desktop)
    } else {
        displays
            .iter()
            .find(|display| display.device_name == settings.video_device_name)
    }?;
    // Only the capture backend may assert its encoder input size. Desktop
    // coordinates are layout geometry (and are point-sized on Retina), so a
    // legacy host that did not publish this field must remain Unknown.
    display.current_capture_resolution
}

pub(super) fn select_video_for_offer(
    requested_setting: Option<&str>,
    client_codecs: &[MediaCodec],
    capabilities: &MediaCapabilities,
    source: Option<Resolution>,
) -> Result<Option<NegotiatedVideo>, CustomDeskError> {
    let encoder_capabilities = if capabilities.video_encoder_capabilities.is_empty() {
        capabilities_for_encoder_names(&capabilities.video_encoders)
    } else {
        capabilities.video_encoder_capabilities.clone()
    };
    let installed = |id: VideoEncoderId| {
        capabilities
            .video_encoders
            .iter()
            .any(|name| VideoEncoderId::from_setting_name(name) == Some(id))
    };
    let codec_for = |id: VideoEncoderId| video_encoder_to_media_codec(VideoEncoderType::from(id));

    if let Some(requested_setting) = requested_setting {
        let id = VideoEncoderId::from_setting_name(requested_setting).ok_or_else(|| {
            CustomDeskError::new(
                DeskErrorCode::INVALID_PARAMS,
                &format!("Unknown video encoder {requested_setting}"),
            )
        })?;
        if !installed(id) {
            return Err(CustomDeskError::new(
                DeskErrorCode::FEATURE_UNAVAILABLE,
                &format!("Video encoder {id:?} is not installed on the host"),
            ));
        }
        let codec = codec_for(id);
        if !client_codecs.contains(&codec) {
            return Err(CustomDeskError::new(
                DeskErrorCode::FEATURE_UNAVAILABLE,
                &format!("The controller cannot decode {codec:?} from encoder {id:?}"),
            ));
        }
        if let Some(source) = source
            && let Some(capability) = encoder_capabilities.iter().find(|item| item.id == id)
            && let Err(reason) = check_encoder_input(source, &capability.input_support)
        {
            return Err(CustomDeskError::new(
                DeskErrorCode::VIDEO_ENCODER_DIMENSIONS_UNSUPPORTED,
                &format!(
                    "Encoder {id:?} does not support {}x{}: {reason:?}",
                    source.width, source.height
                ),
            ));
        }
        return Ok(Some(NegotiatedVideo { codec, encoder: id }));
    }

    let selected = AUTO_ENCODER_ORDER.iter().copied().find(|id| {
        if !installed(*id) || !client_codecs.contains(&codec_for(*id)) {
            return false;
        }
        let Some(capability) = encoder_capabilities.iter().find(|item| item.id == *id) else {
            return false;
        };
        match source {
            Some(source) => {
                check_encoder_input(source, &capability.input_support)
                    == Ok(EncoderCompatibility::Compatible)
            }
            None => matches!(
                capability.input_support,
                desk_signal_facade::model::media_capability::EncoderInputSupport::Known(_)
            ),
        }
    });
    Ok(selected.map(|encoder| NegotiatedVideo {
        codec: codec_for(encoder),
        encoder,
    }))
}

/// Daemon side of `SignalingType::Offer`. Adds video / audio tracks
/// (when the offer SDP carries the matching m-lines) before running
/// the SDP exchange so the answer comes back with proper media
/// directions; the tracks are then fed from the worker.
pub async fn handle_offer(
    registry: &PcRegistry,
    outbound: &OutboundSink,
    worker_mgr: &WorkerManager,
    model: &SignalingModel,
) -> Result<(), DeskError> {
    let from_connection_id = model.check_and_get_from_connection_id()?;
    let offer = model.get_data::<OfferModel>()?;

    let ctx = registry.get(from_connection_id).await.ok_or_else(|| {
        DeskError::CustomError(CustomDeskError::new(
            DeskErrorCode::SYSTEM_ERROR,
            &format!("No PC for {from_connection_id} (offer arrived before RequestRemoteAccess?)"),
        ))
    })?;

    let mut ctx_guard = ctx.write().await;
    if offer.connection_epoch != ctx_guard.connection_epoch {
        return Err(DeskError::CustomError(CustomDeskError::new(
            DeskErrorCode::ACTION_NEED_RETRY,
            "Offer belongs to a stale logical PeerConnection",
        )));
    }
    let desk_settings = offer
        .session_settings
        .as_ref()
        .map(|session| session.merge_into_host_settings(&ctx_guard.host_settings));

    {
        // Apply the browser's adaptive-bitrate preference for this
        // connection before the RTCP reader spawns, so the first REMB
        // decision already sees the right flag. On renegotiation a
        // disable edge with an active cap ships the Clear right here.
        let adaptive = Arc::clone(&ctx_guard.adaptive_bitrate);
        let mut state = adaptive.state.lock().await;
        if let Some(directive) = state.set_enabled_and_decide_clear(
            desk_settings
                .as_ref()
                .is_some_and(|settings| settings.adaptive_bitrate),
        ) {
            let video_generation = ctx_guard
                .cached_start_media
                .read()
                .await
                .as_ref()
                .map_or(1, |payload| payload.video_generation);
            send_cap_directive(
                worker_mgr,
                from_connection_id,
                &ctx_guard.connection_epoch,
                video_generation,
                directive,
                &mut state,
            )
            .await;
        }
    }

    let sdp_str = &offer.offer.sdp;
    let has_video = sdp_str.contains("m=video");
    let has_audio = sdp_str.contains("m=audio");
    if (has_video || has_audio) && desk_settings.is_none() {
        return Err(DeskError::CustomError(CustomDeskError::new(
            DeskErrorCode::INVALID_PARAMS,
            "Remote desktop Offer requires session_settings object",
        )));
    }
    if !has_video && !has_audio && desk_settings.is_some() {
        return Err(DeskError::CustomError(CustomDeskError::new(
            DeskErrorCode::INVALID_PARAMS,
            "DataChannel-only Offer requires session_settings:null",
        )));
    }
    let effective_settings = desk_settings.unwrap_or_else(|| ctx_guard.host_settings.clone());
    log::info!(
        "[pc_manager] Offer from {from_connection_id}: has_video={has_video}, has_audio={has_audio}"
    );
    // Observe only: record the remote SDP's advertised
    // `a=max-message-size` and assert chunk_size + binary-header fits.
    // webrtc-rs 0.17.1 does not expose the negotiated value on
    // `RTCSctpTransport::get_capabilities()` (it currently hard-codes
    // `0`), so we parse the SDP text directly. The check is informational
    // only — a violation logs at `error!` but does NOT block the offer;
    // the actual `dc.send` will surface the failure via
    // `FileTransferSendErrorKind::PacketTooLarge` anyway, but
    // having the warning at SDP time means we catch it before the first
    // byte of file data hits the channel.
    log_sdp_max_message_size(from_connection_id, sdp_str);

    // Freeze a concrete encoder before adding the SDP track. An explicit
    // setting may use a RuntimeProbeRequired implementation (the worker probes
    // that exact encoder); Auto considers only Known-compatible implementations
    // in its stable order, so a worker-side probe can never change the wire
    // codec after the answer is committed.
    let video_encoder_names = list_video_encoder();
    let media_capabilities =
        worker_mgr
            .worker_capabilities()
            .unwrap_or_else(|| MediaCapabilities {
                video_encoders: video_encoder_names.clone(),
                video_encoder_capabilities: capabilities_for_encoder_names(&video_encoder_names),
                ..Default::default()
            });
    let source_resolution = selected_capture_resolution(&effective_settings, &media_capabilities);
    let client_codecs = codec_negotiation::parse_offer_video_codecs(sdp_str);
    let mut video_block = None;
    let negotiated_video = if has_video {
        match select_video_for_offer(
            effective_settings.video_encoder.as_deref(),
            &client_codecs,
            &media_capabilities,
            source_resolution,
        ) {
            Ok(video) => video,
            Err(error) if is_video_offer_blocking_error(error.error_code) => {
                // An unavailable encoder, unsupported wire codec, or
                // unsupported dimensions are video pipeline states rather
                // than whole-SDP failures. Continue the answer without a
                // video sender so audio and DataChannels remain usable.
                video_block = Some(error);
                None
            }
            Err(error) => return Err(DeskError::CustomError(error)),
        }
    } else {
        None
    };
    log::info!(
        "[pc_manager] Video decision for {from_connection_id}: selected={negotiated_video:?}, \
         source={source_resolution:?}, client={client_codecs:?}, requested={:?}",
        effective_settings.video_encoder,
    );

    let previous_start_media = ctx_guard.cached_start_media.read().await.clone();
    if renegotiation_requires_full_reconnect(
        ctx_guard.video_track.is_some(),
        previous_start_media
            .as_ref()
            .map(|payload| payload.video_codec),
        negotiated_video.map(|video| video.codec),
    ) {
        return Err(DeskError::CustomError(CustomDeskError::new(
            DeskErrorCode::VIDEO_PIPELINE_RENEGOTIATION_REQUIRED,
            "Changing the wire video codec requires a full remote-session reconnect",
        )));
    }
    let pipeline_was_retryable = matches!(
        ctx_guard
            .media_pipeline_state
            .read()
            .await
            .as_ref()
            .map(|state| state.phase),
        Some(MediaPipelinePhase::Blocked | MediaPipelinePhase::Failed)
    );

    if let Some(negotiated_video) = negotiated_video
        && ctx_guard.video_track.is_none()
    {
        let video_mime_type = match negotiated_video.codec {
            MediaCodec::H264 => MIME_TYPE_H264,
            MediaCodec::Vp8 => MIME_TYPE_VP8,
            MediaCodec::Vp9 => MIME_TYPE_VP9,
            MediaCodec::Av1 => MIME_TYPE_AV1,
            // Opus is audio-only; the negotiation never yields it for video.
            MediaCodec::Opus => MIME_TYPE_H264,
        };
        let video_track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: video_mime_type.to_owned(),
                ..Default::default()
            },
            "video".to_owned(),
            "webrtc-rs".to_owned(),
        ));
        let rtp_sender = ctx_guard
            .pc
            .add_track(Arc::clone(&video_track) as Arc<dyn TrackLocal + Send + Sync>)
            .await?;
        ctx_guard.video_track = Some(video_track);
        // Spawn the RTCP reader. PLI / FIR from the browser become
        // ForceKeyframe IPC; REMB estimates feed the per-connection
        // adaptive bitrate-cap controller. Reader exits when the
        // rtp_sender is closed (PC drop / CloseRemoteSession), see
        // `spawn_rtcp_feedback_task`.
        spawn_rtcp_feedback_task(
            rtp_sender,
            from_connection_id.to_string(),
            ctx_guard.connection_epoch.clone(),
            worker_mgr.clone(),
            Arc::clone(&ctx_guard.adaptive_bitrate),
            Arc::clone(&ctx_guard.media_coordinator),
        );
    }

    if has_audio && ctx_guard.audio_track.is_none() {
        let audio_track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_OPUS.to_owned(),
                channels: 2,
                ..Default::default()
            },
            "audio".to_owned(),
            "webrtc-rs".to_owned(),
        ));
        let _rtp_sender = ctx_guard
            .pc
            .add_track(Arc::clone(&audio_track) as Arc<dyn TrackLocal + Send + Sync>)
            .await?;
        ctx_guard.audio_track = Some(audio_track);
    }

    ctx_guard.pc.set_remote_description(offer.offer).await?;
    let answer = ctx_guard.pc.create_answer(None).await?;
    ctx_guard.pc.set_local_description(answer).await?;

    if let Some(local_desc) = ctx_guard.pc.local_description().await {
        log::info!("[pc_manager] Sending Answer for {from_connection_id}");
        send_response(
            outbound,
            &model.request_id,
            SignalingType::Answer,
            from_connection_id,
            Some(&local_desc),
        )?;
    }

    // Now that the SDP exchange has populated tracks, tell the worker
    // to start its per-`connection_id` encoder. Without this the daemon
    // would have a video_track that nobody ever feeds. The codec is the
    // one negotiated above (client-decodable ∩ host-encodable) so the
    // worker's encoder and the daemon's track always agree. Audio codec
    // is currently fixed to Opus.
    let requested_encoder = effective_settings
        .video_encoder
        .as_deref()
        .and_then(VideoEncoderId::from_setting_name);
    let payload_video = negotiated_video.or_else(|| {
        requested_encoder.map(|encoder| NegotiatedVideo {
            codec: video_encoder_to_media_codec(VideoEncoderType::from(encoder)),
            encoder,
        })
    });
    let video_codec = payload_video
        .map(|video| video.codec)
        .unwrap_or(MediaCodec::H264);
    // v4 capture-selection fix: thread the browser-chosen GDI device
    // name through to the worker so capture binds to the right
    // monitor. See [`video_device_for_payload`] for the empty-string
    // semantics (legal-but-unselected fresh install case).
    let video_device = video_device_for_payload(&effective_settings.video_device_name);
    let resolved_wayland_control_mode = ctx_guard
        .signaling_state
        .read()
        .await
        .resolved_wayland_control_mode;
    let requested_audio = has_audio
        .then(|| offer.session_settings.as_ref()?.audio.clone())
        .flatten();
    let start_media_payload = StartMediaPayload {
        connection_id: from_connection_id.to_string(),
        connection_epoch: ctx_guard.connection_epoch.clone(),
        video_generation: 1,
        audio_generation: 1,
        video_codec,
        video_encoder: payload_video.map_or(VideoEncoderId::X264, |video| video.encoder),
        video_device,
        fps: effective_settings.video_fps,
        bitrate_kbps: 0,
        quality: effective_settings.video_quality,
        // Track presence in the offer drives whether the worker spawns
        // each capture pipeline. The browser file-management page
        // negotiates a DataChannel-only PC (no `m=video`, no `m=audio`)
        // and must not trigger DXGI / WASAPI capture — see the worker
        // `start_media` doc comment for the rationale.
        // For an explicit incompatible encoder, cache the desired video start
        // so a later display-mode change or encoder selection can reuse the
        // single restart primitive. The first command sent below is changed to
        // audio-only while the pipeline is blocked.
        start_video: has_video && (negotiated_video.is_some() || video_block.is_some()),
        // Answer/PC setup must not wait for a local approval prompt. Initial
        // media is video-only; the router starts audio asynchronously after
        // the Answer is already on the wire.
        audio: None,
        // Per-connection backend choice — propagating it lets a
        // second browser pick a different backend (e.g. one DXGI +
        // one GDI) without colliding on the first connection's
        // DuplicateOutput. The worker falls back to its own settings
        // when this is `None`.
        image_capture: effective_settings
            .image_capture
            .clone()
            .unwrap_or_else(|| "default".to_string()),
        resolved_wayland_control_mode,
        // Thread the Advanced-tab dirty-rect kill-switch from the
        // browser offer through to the worker on the *first*
        // StartMedia. Without this the worker's `merged_settings`
        // would always pick up its base-settings default (`true`),
        // regardless of what the browser actually negotiated, and the
        // toggle would only take effect after a subsequent 301 apply.
        enable_dirty_rect: effective_settings.enable_dirty_rect,
        show_mouse: effective_settings.show_mouse,
    };
    // Record the payload + decide first-vs-renegotiation while still
    // holding `ctx_guard`, so two concurrent offers for the same
    // connection (an in-flight initial offer racing a frontend
    // ICE-restart re-offer) cannot both observe an empty cache and
    // double-issue StartMedia. Publishing here also keeps the cache
    // ahead of any worker-swap `resume_active_media` that races the swap
    // (it reads the cache under `ctx.read()`).
    let mut initial_baseline = offer.session_settings.clone();
    if let Some(settings) = initial_baseline.as_mut() {
        settings.audio = None;
    }
    let is_first_offer = ctx_guard
        .install_initial_media(start_media_payload.clone(), initial_baseline)
        .await;
    if is_first_offer {
        ctx_guard
            .media_coordinator
            .lock()
            .await
            .pending_audio_candidate = requested_audio;
        let mut fence = ctx_guard.media_output_fence.write().await;
        fence.video_epoch = ctx_guard.connection_epoch.clone();
        fence.video_generation = start_media_payload.video_generation;
        fence.audio_epoch = ctx_guard.connection_epoch.clone();
        fence.audio_generation = start_media_payload.audio_generation;
        fence.audio_open = false;
    }
    let restart_after_renegotiation = should_restart_negotiated_pipeline(
        is_first_offer,
        pipeline_was_retryable,
        has_video,
        negotiated_video,
    );
    if restart_after_renegotiation {
        // Reserve this recovery until the worker publishes its next terminal or
        // Streaming state. A duplicate Retry click cannot race a second restart.
        *ctx_guard.media_pipeline_state.write().await = None;
    }
    let connection_epoch = ctx_guard.connection_epoch.clone();
    drop(ctx_guard);
    if has_video
        && negotiated_video.is_some()
        && let Some(activity) = registry.host_activity()
    {
        activity.mark_video_negotiated(from_connection_id);
    }
    if has_video && negotiated_video.is_none() {
        let encoder_capabilities = if media_capabilities.video_encoder_capabilities.is_empty() {
            capabilities_for_encoder_names(&media_capabilities.video_encoders)
        } else {
            media_capabilities.video_encoder_capabilities.clone()
        };
        let compatible = source_resolution
            .map(|source| {
                desk_signal_facade::model::media_capability::compatible_encoders(
                    source,
                    &encoder_capabilities,
                )
            })
            .unwrap_or_default()
            .into_iter()
            .filter(|id| {
                media_capabilities
                    .video_encoders
                    .iter()
                    .any(|name| VideoEncoderId::from_setting_name(name) == Some(*id))
                    && client_codecs
                        .contains(&video_encoder_to_media_codec(VideoEncoderType::from(*id)))
            })
            .collect();
        let data = blocked_offer_state(
            requested_encoder,
            source_resolution,
            compatible,
            video_block.as_ref(),
        );
        registry
            .record_media_pipeline_state(
                from_connection_id,
                &connection_epoch,
                start_media_payload.video_generation,
                data.clone(),
            )
            .await;
        if let Ok(state_model) = SignalingModel::new_request(
            SignalingType::MediaPipelineStateChanged,
            Some(from_connection_id.to_string()),
            Some(&data),
        ) && let Ok(text) = serde_json::to_string(&state_model)
        {
            let _ = outbound.send(text);
        }
    }
    // Only the first offer starts the worker's per-connection capture +
    // encode pipeline. A renegotiation (ICE-restart re-offer) finished
    // the SDP exchange above but must not re-issue StartMedia.
    if is_first_offer
        && let Err(e) = worker_mgr
            .send_to_worker(ServiceToWorker::StartMedia({
                let mut initial_payload = start_media_payload;
                let (start_video, start_audio) = initial_worker_media_flags(
                    initial_payload.start_video,
                    initial_payload.audio.is_some(),
                    has_video && negotiated_video.is_none(),
                );
                initial_payload.start_video = start_video;
                if !start_audio {
                    initial_payload.audio = None;
                }
                initial_payload
            }))
            .await
    {
        log::warn!(
            "[pc_manager] Failed to issue StartMedia to worker for {from_connection_id}: {e} \
             (PC is up but no media will flow until worker comes online)"
        );
    }
    if restart_after_renegotiation {
        match registry
            .restart_media_from_cached_payload(
                from_connection_id,
                worker_mgr,
                MediaRestartTrigger::RenegotiatedSettings,
            )
            .await
        {
            RestartOutcome::Restarted => {}
            RestartOutcome::NoCachedPayload { left_paused } => {
                let data = MediaPipelineStateData {
                    phase: MediaPipelinePhase::Blocked,
                    encoder: negotiated_video.map(|video| video.encoder),
                    source_resolution,
                    compatible_encoders: Vec::new(),
                    reason_code: Some(DeskErrorCode::VIDEO_PIPELINE_RENEGOTIATION_REQUIRED),
                    message: Some(format!(
                        "renegotiated media has no cached payload; left_paused={left_paused}"
                    )),
                };
                publish_offer_media_state(registry, outbound, from_connection_id, data).await;
            }
            RestartOutcome::Failed { stage } => {
                let data = MediaPipelineStateData {
                    phase: MediaPipelinePhase::Failed,
                    encoder: negotiated_video.map(|video| video.encoder),
                    source_resolution,
                    compatible_encoders: Vec::new(),
                    reason_code: Some(DeskErrorCode::VIDEO_PIPELINE_RESTART_FAILED),
                    message: Some(format!("renegotiated media restart failed at {stage:?}")),
                };
                publish_offer_media_state(registry, outbound, from_connection_id, data).await;
            }
        }
    }
    Ok(())
}

async fn publish_offer_media_state(
    registry: &PcRegistry,
    outbound: &OutboundSink,
    connection_id: &str,
    data: MediaPipelineStateData,
) {
    if let Some(pc) = registry.get(connection_id).await {
        let pc = pc.read().await;
        let connection_epoch = pc.connection_epoch.clone();
        let generation = pc
            .cached_start_media
            .read()
            .await
            .as_ref()
            .map_or(0, |payload| payload.video_generation);
        drop(pc);
        registry
            .record_media_pipeline_state(connection_id, &connection_epoch, generation, data.clone())
            .await;
    }
    if let Ok(model) = SignalingModel::new_request(
        SignalingType::MediaPipelineStateChanged,
        Some(connection_id.to_string()),
        Some(&data),
    ) && let Ok(text) = serde_json::to_string(&model)
    {
        let _ = outbound.send(text);
    }
}

/// Daemon side of `SignalingType::IceCandidate` (ICE candidate). Mirrors the
/// worker's mDNS rewrite path for `*.local` hosts.
pub async fn handle_ice_candidate(
    registry: &PcRegistry,
    model: &SignalingModel,
) -> Result<(), DeskError> {
    let from_connection_id = model.check_and_get_from_connection_id()?;
    let ctx = registry.get(from_connection_id).await.ok_or_else(|| {
        DeskError::CustomError(CustomDeskError::new(
            DeskErrorCode::SYSTEM_ERROR,
            &format!("No PC for {from_connection_id} (IceCandidate before RequestRemoteAccess?)"),
        ))
    })?;
    let payload = match model
        .get_data_with_type::<desk_signal_facade::model::remote_session::IceCandidatePayload>()?
    {
        Some(payload) => payload,
        None => return Ok(()),
    };
    {
        let guard = ctx.read().await;
        if payload.connection_epoch != guard.connection_epoch {
            log::debug!("[pc_manager] dropping stale ICE candidate for {from_connection_id}");
            return Ok(());
        }
    }
    let mut candidate_init = payload.candidate;
    log::info!(
        "[pc_manager] ICE candidate for {from_connection_id}: candidate=\"{}\" sdp_mid={:?} \
         sdp_mline_index={:?} ufrag={:?}",
        candidate_init.candidate,
        candidate_init.sdp_mid,
        candidate_init.sdp_mline_index,
        candidate_init.username_fragment,
    );
    if candidate_init.candidate.contains(".local") {
        let mut parts = candidate_init
            .candidate
            .split_whitespace()
            .map(|s| s.to_string())
            .collect::<Vec<_>>();
        if parts.len() >= 6 {
            let host = parts[4].clone();
            if host.ends_with(".local")
                && let Some(ip) = crate::service::signaling::resolve_mdns_host(&host).await
            {
                log::info!("[pc_manager] Resolved mDNS {host} -> {ip}");
                parts[4] = ip.to_string();
                candidate_init.candidate = parts.join(" ");
            }
        }
    }
    let ctx = ctx.read().await;
    if let Err(e) = ctx.pc.add_ice_candidate(candidate_init).await {
        log::warn!("[pc_manager] add_ice_candidate failed: {e}");
    }
    Ok(())
}
