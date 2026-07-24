//! SDP offer handling and media negotiation.

use super::*;

/// Inverse of the worker-side codec mapping. Used by the Init reply
/// path so the daemon's `audio_encoder_list` / `video_encoder_list`
/// payloads carry the same string identifiers the legacy worker did.
pub(super) fn media_codec_to_str(c: &MediaCodec) -> Option<String> {
    match c {
        MediaCodec::H264 => Some("H264".to_string()),
        MediaCodec::Vp8 => Some("VP8".to_string()),
        MediaCodec::Vp9 => Some("VP9".to_string()),
        MediaCodec::Av1 => Some("AV1".to_string()),
        MediaCodec::Opus => Some("OPUS".to_string()),
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
            &format!("No PC for {from_connection_id} (offer arrived before RequestRemote?)"),
        ))
    })?;

    let mut ctx_guard = ctx.write().await;

    {
        let mut s = ctx_guard.signaling_state.write().await;
        s.wayland_control_mode = offer.desk_settings.wayland_control_mode.clone();
    }

    {
        // Apply the browser's adaptive-bitrate preference for this
        // connection before the RTCP reader spawns, so the first REMB
        // decision already sees the right flag. On renegotiation a
        // disable edge with an active cap ships the Clear right here.
        let adaptive = Arc::clone(&ctx_guard.adaptive_bitrate);
        let mut state = adaptive.state.lock().await;
        if let Some(directive) =
            state.set_enabled_and_decide_clear(offer.desk_settings.adaptive_bitrate)
        {
            send_cap_directive(worker_mgr, from_connection_id, directive, &mut state).await;
        }
    }

    let sdp_str = &offer.offer.sdp;
    let has_video = sdp_str.contains("m=video");
    let has_audio = sdp_str.contains("m=audio");
    log::info!(
        "[pc_manager] Offer from {from_connection_id}: has_video={has_video}, has_audio={has_audio}"
    );
    // F3 (observe-only): record the remote SDP's advertised
    // `a=max-message-size` and assert chunk_size + binary-header fits.
    // webrtc-rs 0.17.1 does not expose the negotiated value on
    // `RTCSctpTransport::get_capabilities()` (it currently hard-codes
    // `0`), so we parse the SDP text directly. The check is informational
    // only — a violation logs at `error!` but does NOT block the offer;
    // the actual `dc.send` will surface the failure via
    // F1/F2 (`FileTransferSendErrorKind::PacketTooLarge`) anyway, but
    // having the warning at SDP time means we catch it before the first
    // byte of file data hits the channel.
    log_sdp_max_message_size(from_connection_id, sdp_str);

    // Negotiate the single video codec the host will encode for this
    // connection: intersect the codecs the client advertised it can decode
    // (the offer's `m=video` rtpmap) with the codecs the host can encode,
    // honouring `desk_settings.video_encoder` as a preference hint. This
    // replaces the legacy "trust the client-asserted codec verbatim" path
    // so a client never receives a codec it cannot decode (black screen).
    // Falls back to the configured default only when no codec is shared,
    // which is effectively impossible since VP8 is a universal baseline.
    let preferred_codec = offer
        .desk_settings
        .get_video_encoder_type()
        .ok()
        .map(video_encoder_to_media_codec);
    let negotiated_video_codec = if has_video {
        let client_codecs = codec_negotiation::parse_offer_video_codecs(sdp_str);
        let server_codecs = codec_negotiation::server_encodable_video_codecs();
        match codec_negotiation::negotiate_video_codec(
            &client_codecs,
            &server_codecs,
            preferred_codec,
        ) {
            Some(codec) => {
                log::info!(
                    "[pc_manager] Negotiated video codec {codec:?} for {from_connection_id} \
                     (client={client_codecs:?}, preferred={preferred_codec:?})"
                );
                codec
            }
            None => {
                let fallback = preferred_codec.unwrap_or(MediaCodec::H264);
                log::warn!(
                    "[pc_manager] No video codec shared with {from_connection_id} \
                     (client={client_codecs:?}, server={server_codecs:?}); falling back to \
                     {fallback:?} — the client may be unable to decode"
                );
                fallback
            }
        }
    } else {
        preferred_codec.unwrap_or(MediaCodec::H264)
    };

    if has_video && ctx_guard.video_track.is_none() {
        let video_mime_type = match negotiated_video_codec {
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
        // rtp_sender is closed (PC drop / CloseControl), see
        // `spawn_rtcp_feedback_task`.
        spawn_rtcp_feedback_task(
            rtp_sender,
            from_connection_id.to_string(),
            worker_mgr.clone(),
            Arc::clone(&ctx_guard.adaptive_bitrate),
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
    // is currently fixed to OPUS.
    let video_codec = negotiated_video_codec;
    // v4 capture-selection fix: thread the browser-chosen GDI device
    // name through to the worker so capture binds to the right
    // monitor. See [`video_device_for_payload`] for the empty-string
    // semantics (legal-but-unselected fresh install case).
    let video_device = video_device_for_payload(&offer.desk_settings.video_device_name);
    let start_media_payload = StartMediaPayload {
        connection_id: from_connection_id.to_string(),
        video_codec,
        audio_codec: MediaCodec::Opus,
        video_device,
        audio_device: None,
        fps: offer.desk_settings.video_fps,
        bitrate_kbps: 0,
        quality: offer.desk_settings.video_quality,
        // Track presence in the offer drives whether the worker spawns
        // each capture pipeline. The browser file-management page
        // negotiates a DataChannel-only PC (no `m=video`, no `m=audio`)
        // and must not trigger DXGI / WASAPI capture — see the worker
        // `start_media` doc comment for the rationale.
        start_video: has_video,
        start_audio: has_audio,
        // Per-connection backend choice — propagating it lets a
        // second browser pick a different backend (e.g. one DXGI +
        // one GDI) without colliding on the first connection's
        // DuplicateOutput. The worker falls back to its own settings
        // when this is `None`.
        image_capture: offer.desk_settings.image_capture.clone(),
        // Thread the Advanced-tab dirty-rect kill-switch from the
        // browser offer through to the worker on the *first*
        // StartMedia. Without this the worker's `merged_settings`
        // would always pick up its base-settings default (`true`),
        // regardless of what the browser actually negotiated, and the
        // toggle would only take effect after a subsequent live
        // `UpdateDeskSettings` round-trip.
        enable_dirty_rect: Some(offer.desk_settings.enable_dirty_rect),
    };
    // Record the payload + decide first-vs-renegotiation while still
    // holding `ctx_guard`, so two concurrent offers for the same
    // connection (an in-flight initial offer racing a frontend
    // ICE-restart re-offer) cannot both observe an empty cache and
    // double-issue StartMedia. Publishing here also keeps the cache
    // ahead of any worker-swap `resume_active_media` that races the swap
    // (it reads the cache under `ctx.read()`).
    let is_first_offer = ctx_guard
        .record_start_media_was_first(start_media_payload.clone())
        .await;
    drop(ctx_guard);
    if has_video && let Some(activity) = registry.host_activity() {
        activity.mark_video_negotiated(from_connection_id);
    }
    // Only the first offer starts the worker's per-connection capture +
    // encode pipeline. A renegotiation (ICE-restart re-offer) finished
    // the SDP exchange above but must not re-issue StartMedia.
    if is_first_offer
        && let Err(e) = worker_mgr
            .send_to_worker(ServiceToWorker::StartMedia(start_media_payload))
            .await
    {
        log::warn!(
            "[pc_manager] Failed to issue StartMedia to worker for {from_connection_id}: {e} \
             (PC is up but no media will flow until worker comes online)"
        );
    }
    Ok(())
}

/// Daemon side of `SignalingType::Canid` (ICE candidate). Mirrors the
/// worker's mDNS rewrite path for `*.local` hosts.
pub async fn handle_canid(registry: &PcRegistry, model: &SignalingModel) -> Result<(), DeskError> {
    let from_connection_id = model.check_and_get_from_connection_id()?;
    let ctx = registry.get(from_connection_id).await.ok_or_else(|| {
        DeskError::CustomError(CustomDeskError::new(
            DeskErrorCode::SYSTEM_ERROR,
            &format!("No PC for {from_connection_id} (Canid before RequestRemote?)"),
        ))
    })?;
    let mut candidate_init = match model.get_data_with_type::<RTCIceCandidateInit>()? {
        Some(c) => c,
        None => return Ok(()),
    };
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
