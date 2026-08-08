use super::*;

/// Inner async loop for video. Subscribes to the worker-wide
/// `SharedCaptureRegistry` for its `(backend, output_index)` and
/// pumps frames from the broadcast channel into a per-connection
/// encoder. fps is honoured by per-connection throttling — the
/// shared capture loop runs at the OS refresh rate; this loop drops
/// frames when its own quality knob asks for a lower rate. Heartbeat-
/// frame behaviour: on a static desktop emit one cached frame per
/// second so the receiver does not stall.
pub(super) async fn video_pipeline_loop(
    base_settings: DeskSettings,
    payload: StartMediaPayload,
    media_sender: Arc<dyn MediaSender>,
    error_tx: mpsc::UnboundedSender<WorkerToService>,
    stop_flag: Arc<AtomicBool>,
    mut stop_rx: watch::Receiver<bool>,
    keyframe_requested: Arc<AtomicBool>,
    mut settings_rx: mpsc::UnboundedReceiver<UpdateMediaSettingsPayload>,
    capture_registry: Arc<SharedCaptureRegistry>,
    capture_keys: Arc<StdMutex<HashMap<String, CaptureKeyRecord>>>,
    generation: u64,
    geometry_update_handler: Option<Arc<GeometryUpdateHandler>>,
    video_state: Arc<AtomicU8>,
) -> Result<(), String> {
    let connection_id = payload.connection_id.clone();
    let codec = payload.video_codec;
    let mut merged_settings = payload_overrides(&base_settings, &payload);

    info!(
        "[MediaProducer:{connection_id}] Starting pipeline: codec={codec:?}, fps={}, \
         enable_dirty_rect={}",
        merged_settings.video_fps, merged_settings.enable_dirty_rect
    );

    // Guard against a stale capability snapshot: an IDD virtual display can be
    // advertised in INIT (and chosen by the client) yet be gone by capture
    // time. Substitute a live, capturable display instead of hard-erroring,
    // mirroring the input dispatcher's geometry fallback.
    if !merged_settings.video_device_name.is_empty()
        && let Ok(live) = list_effective_image_output(&merged_settings)
        && let Some(name) = capturable_device_name(&live, &merged_settings.video_device_name)
        && name != merged_settings.video_device_name
    {
        warn!(
            "[MediaProducer:{connection_id}] requested display {:?} not live; \
             falling back to {:?}",
            merged_settings.video_device_name, name
        );
        merged_settings.video_device_name = name;
    }

    // Subscribe to the shared capture loop for this `(backend,
    // output)`. If no loop exists the registry spawns one;
    // otherwise the existing loop's broadcast sender hands us a
    // fresh receiver. `display_info` is published by the registry
    // (the capture instance owns it) so we don't need our own
    // capture handle to re-derive resolution.
    let capture_handle = capture_registry
        .subscribe(&merged_settings)
        .map_err(|e| format!("{e}"))?;

    // Publish the effective `CaptureKey` so `SetVirtualDisplayMode`
    // can decide whether this connection's backend is WGC (needs a
    // forced rebuild after IddCx remount) or one that self-adapts
    // (DXGI / GDI). The RAII guard below ensures we clean up on every
    // exit path: normal return, `?`-propagated encoder error, or panic
    // unwind — without it a subscribe-time success followed by a later
    // failure would leak the entry past the connection's lifetime.
    //
    // The generation tag is what makes the cleanup safe in the face
    // of a Stop+Start race. `stop_media` does not block-join the
    // outgoing thread, so the *next* `start_media` for the same
    // connection_id may spawn a new pipeline that finishes subscribe
    // and overwrites this entry before the old thread's stack
    // unwinds. Tagging the record with the generation we just bumped
    // — and re-checking it in `CaptureKeyGuard::drop` — lets the old
    // guard recognise "the slot no longer belongs to me, leave it
    // alone." Without this token the old guard would erase the new
    // pipeline's freshly recorded key, and the next
    // `SetVirtualDisplayMode` would silently skip the WGC restart.
    capture_keys
        .lock()
        .expect("media producer capture_keys lock poisoned")
        .insert(
            connection_id.clone(),
            CaptureKeyRecord {
                key: capture_handle.key().clone(),
                generation,
            },
        );
    let _capture_key_guard = CaptureKeyGuard {
        map: Arc::clone(&capture_keys),
        connection_id: connection_id.clone(),
        generation,
    };

    let mut frame_rx = capture_handle.subscribe();
    let (mut source_generation, mut display_info) = capture_handle.geometry_snapshot();
    let coordinates = display_info.desktop_coordinates;
    if coordinates.width() > 0
        && coordinates.height() > 0
        && let Some(handler) = geometry_update_handler.as_ref()
    {
        handler(
            &connection_id,
            generation,
            (
                coordinates.left,
                coordinates.top,
                coordinates.width(),
                coordinates.height(),
            ),
        );
    }

    // `encoder_init_size` is the *only* authoritative source of the
    // encoder's current width/height. Every `create_video_encoder`
    // call below feeds through `display_info_for_size(&display_info,
    // encoder_init_size)` so settings_changed / keyframe_requested
    // rebuilds never accidentally drop back to the (stale) subscribe-
    // time resolution after a mid-session display mode change.
    let mut encoder_init_size: (u32, u32) = display_info
        .current_capture_resolution
        .map(|resolution| (resolution.width, resolution.height))
        .unwrap_or_else(|| {
            (
                display_info.desktop_coordinates.width().max(0) as u32,
                display_info.desktop_coordinates.height().max(0) as u32,
            )
        });
    let encoder_id = payload.video_encoder.or_else(|| {
        merged_settings
            .video_encoder
            .as_deref()
            .and_then(VideoEncoderId::from_setting_name)
    });
    if let Err(reason) = preflight_encoder_dimensions(encoder_id, encoder_init_size) {
        video_state.store(VIDEO_STATE_BLOCKED, Ordering::Release);
        report_dimension_blocked(
            &error_tx,
            &connection_id,
            encoder_id,
            encoder_init_size,
            reason,
        );
        return Ok(());
    }
    let mut encoder: Box<dyn VideoEncoder> = match create_video_encoder(
        &merged_settings,
        &display_info_for_size(&display_info, encoder_init_size),
    ) {
        Ok(encoder) => encoder,
        Err(e) => {
            report_prepare_failed(
                &video_state,
                &error_tx,
                &connection_id,
                encoder_id,
                encoder_init_size,
                format!("{e}"),
            );
            return Ok(());
        }
    };
    video_state.store(VIDEO_STATE_STREAMING, Ordering::Release);
    report_streaming(&error_tx, &connection_id, encoder_id, encoder_init_size);
    let mut next_pass_is_idr = true; // first frame is always I (encoder emits SPS/PPS+IDR)
    let mut seq: u64 = 0;
    let mut frame_interval = merged_settings.get_duration_by_video_fps();
    let mut frame_duration_ns = frame_interval.as_nanos().min(u64::MAX as u128) as u64;
    let mut last_send_time = std::time::Instant::now();
    // Wall-clock instant of the most recent emit — drives the
    // dynamic `Sample.duration` calculation. `None` until we've
    // emitted at least once; the first emit uses `frame_duration_ns`
    // (1/fps) as a sensible default since there's no previous tick
    // to subtract.
    let mut last_emit_wall: Option<std::time::Instant> = None;
    // Force the first emitted frame to bypass the throttle gate so
    // the browser sees an IDR immediately on connect (initial
    // `last_emit_for_throttle = now - frame_interval` lets the very
    // first non-heartbeat tick pass).
    let mut last_emit_for_throttle = std::time::Instant::now()
        .checked_sub(frame_interval)
        .unwrap_or_else(std::time::Instant::now);
    // Diagnostic flag: set whenever the encoder is freshly built (initial
    // construction, settings_changed rebuild, or keyframe_requested
    // rebuild). The first emission pass after the rebuild logs a single
    // INFO line describing the resulting NAL layout — used to triage
    // bugs like the "screen turns green after a while" failure.
    let mut rebuild_pending = true;
    // Connection-transient bitrate cap (kbps) driven by the daemon's
    // REMB controller via `UpdateMediaSettings.bitrate_kbps`. Not part
    // of `merged_settings` — it is runtime state, never persisted, and
    // must be replayed onto every freshly rebuilt encoder. `None` =
    // encoder runs at its initial ceiling.
    let mut current_cap_kbps: Option<u32> = None;
    let mut consecutive_encode_failures = 0_u8;

    while !stop_flag.load(Ordering::Relaxed) {
        // Wait for the next shared frame. The capture loop runs as
        // fast as the backend yields; this loop's fps throttle gates
        // whether the frame is encoded or skipped.
        if *stop_rx.borrow() {
            break;
        }
        let next_frame = tokio::select! {
            biased;
            _ = stop_rx.changed() => break,
            frame = frame_rx.recv() => frame,
        };
        let shared_frame = match next_frame {
            Ok(f) => f,
            Err(broadcast::error::RecvError::Closed) => {
                warn!(
                    "[MediaProducer:{connection_id}] shared-capture broadcast closed; pipeline \
                     exiting"
                );
                return Ok(());
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                // The shared-capture loop runs at the OS refresh
                // rate; per-connection encoders run at a (typically
                // lower) configured fps, so the bounded broadcast
                // ring will routinely drop the oldest queued *input*
                // frames before our next recv. This is benign:
                //
                //  - We missed *input* frames, not *output* RTP.
                //  - The encoder's internal reference chain is still
                //    valid (we never fed it a frame after our last
                //    successful encode).
                //  - The next P frame off the latest available input
                //    describes the gap correctly to the browser
                //    without an IDR.
                //
                // Earlier versions requested a keyframe on every lag,
                // which recreated the encoder — an order of magnitude
                // more expensive than emitting one P frame — and fed
                // a self-amplifying keyframe-storm loop where each
                // rebuild widened the lag, triggering more rebuilds.
                handle_broadcast_lag(&connection_id, n);
                continue;
            }
        };
        if stop_flag.load(Ordering::Relaxed) {
            break;
        }

        if shared_frame.source_generation != source_generation {
            source_generation = shared_frame.source_generation;
            display_info = shared_frame.display_info.clone();
            let coordinates = display_info.desktop_coordinates;
            if coordinates.width() > 0
                && coordinates.height() > 0
                && let Some(handler) = geometry_update_handler.as_ref()
            {
                handler(
                    &connection_id,
                    generation,
                    (
                        coordinates.left,
                        coordinates.top,
                        coordinates.width(),
                        coordinates.height(),
                    ),
                );
            }
        }

        // Apply any pending live-update settings before honouring
        // the keyframe flag. Coalesce a burst into a single
        // rebuild. NB: backend / output_index changes are out of
        // scope here — they would require resubscribing to a
        // different `CaptureKey`, and the live-settings stream
        // currently does not include them.
        let drain_outcome = drain_settings_updates(
            &connection_id,
            &mut settings_rx,
            &mut merged_settings,
            &mut frame_interval,
            &mut frame_duration_ns,
        );
        if drain_outcome.needs_rebuild {
            info!(
                "[MediaProducer:{connection_id}] Live settings changed; recreating encoder \
                 (fps={}, video_quality={}, enable_dirty_rect={})",
                merged_settings.video_fps,
                merged_settings.video_quality,
                merged_settings.enable_dirty_rect
            );
            encoder = match create_video_encoder(
                &merged_settings,
                &display_info_for_size(&display_info, encoder_init_size),
            ) {
                Ok(encoder) => encoder,
                Err(error) => {
                    report_prepare_failed(
                        &video_state,
                        &error_tx,
                        &connection_id,
                        encoder_id,
                        encoder_init_size,
                        format!("{error}"),
                    );
                    return Ok(());
                }
            };
            next_pass_is_idr = true;
            rebuild_pending = true;
            // Reset throttle so the new encoder's first IDR is
            // emitted on the very next non-heartbeat frame, not
            // delayed by `frame_interval`.
            last_emit_for_throttle = std::time::Instant::now()
                .checked_sub(frame_interval)
                .unwrap_or_else(std::time::Instant::now);
        }
        // Bitrate-cap directives apply *after* a potential rebuild so
        // a batch carrying both a quality change and a cap lands on
        // the new encoder. Without a fresh directive, a rebuild
        // replays the connection's current cap (the new encoder
        // starts at its initial ceiling).
        match drain_outcome.cap_directive {
            Some(directive) => {
                if !encoder.set_bitrate_cap(directive) {
                    debug!(
                        "[MediaProducer:{connection_id}] bitrate cap directive {directive:?} not \
                         applied (encoder unsupported or reconfig failed)"
                    );
                }
                current_cap_kbps = directive;
            }
            None if drain_outcome.needs_rebuild => {
                replay_bitrate_cap(&mut encoder, current_cap_kbps, &connection_id);
            }
            None => {}
        }

        if keyframe_requested.swap(false, Ordering::Relaxed) {
            info!(
                "[MediaProducer:{connection_id}] Keyframe requested; recreating encoder so the \
                 next encode pass emits an IDR"
            );
            encoder = match create_video_encoder(
                &merged_settings,
                &display_info_for_size(&display_info, encoder_init_size),
            ) {
                Ok(encoder) => encoder,
                Err(error) => {
                    report_prepare_failed(
                        &video_state,
                        &error_tx,
                        &connection_id,
                        encoder_id,
                        encoder_init_size,
                        format!("{error}"),
                    );
                    return Ok(());
                }
            };
            replay_bitrate_cap(&mut encoder, current_cap_kbps, &connection_id);
            next_pass_is_idr = true;
            rebuild_pending = true;
            last_emit_for_throttle = std::time::Instant::now()
                .checked_sub(frame_interval)
                .unwrap_or_else(std::time::Instant::now);
        }

        // Cursor sync: the shared capture loop is hard-pinned to
        // SyncNative, so cursor metadata is always present (when the
        // backend has an update). Per-connection `show_mouse`
        // decides whether to forward it on this connection's
        // dedicated `cursor_sync_event` DC. This is how two browsers
        // sharing a capture can independently choose to display or
        // suppress the cursor.
        if merged_settings.show_mouse
            && let Some(cursor) = &shared_frame.cursor_update
        {
            match serde_json::to_vec(cursor) {
                Ok(bytes) => {
                    let payload = desk_ipc_protocol::message::CursorDataPayload {
                        connection_id: connection_id.clone(),
                        data: bytes,
                    };
                    if error_tx.send(WorkerToService::CursorData(payload)).is_err() {
                        debug!(
                            "[MediaProducer:{connection_id}] event pipe closed; \
                             cursor IPC will not flow"
                        );
                    }
                }
                Err(e) => {
                    warn!("[MediaProducer:{connection_id}] failed to serialise cursor update: {e}");
                }
            }
        }

        let now = std::time::Instant::now();

        if !shared_frame.content_changed {
            // Static-desktop heartbeat: emit one cached frame per
            // second so the daemon-side track keeps producing RTP
            // and the browser decoder does not declare the stream
            // dead. Heartbeats bypass the fps throttle (one per
            // second is well below any sensible fps anyway).
            if last_send_time.elapsed() <= Duration::from_secs(1) {
                continue;
            }
            let nal_info_vec = match encoder.encode_cached() {
                Ok(v) => {
                    consecutive_encode_failures = 0;
                    v
                }
                Err(e) => {
                    if record_encode_failure(&mut consecutive_encode_failures) {
                        report_runtime_failed(
                            &video_state,
                            &error_tx,
                            &connection_id,
                            encoder_id,
                            encoder_init_size,
                            format!("encode_cached failed three consecutive times: {e}"),
                        );
                        return Ok(());
                    }
                    warn!(
                        "[MediaProducer:{connection_id}] transient encode_cached error \
                         ({consecutive_encode_failures}/3): {e}"
                    );
                    continue;
                }
            };
            if rebuild_pending && nal_info_vec.is_empty() {
                warn!(
                    "[MediaProducer:{connection_id}] post-rebuild heartbeat tick produced 0 \
                     NALs (encoder yuv_buffer is None on a freshly built encoder); browser \
                     will see no frames until the next non-static capture tick"
                );
            }
            // Heartbeat duration must reflect wall-clock elapsed
            // (~1s under the static-desktop branch above) so the
            // receiver's RTP timestamps stay in sync with wall
            // clock. Subsequent NALs from the same encode pass
            // share the timestamp (duration=0) — they describe
            // the same access unit.
            let actual_duration_ns =
                compute_emit_duration_ns(last_emit_wall, now, frame_duration_ns);
            // Honour the encoder's native frame-type signal: an
            // internal-GOP IDR mid-heartbeat must surface as VideoI so
            // the daemon's paused-write_sample latch (after a worker
            // swap) can clear on a natural IDR, and so host-side I-frame
            // counts match what the browser decoder reports.
            let kind_for_pass = classify_video_frame_kind(&nal_info_vec, next_pass_is_idr);
            let was_idr_flag = next_pass_is_idr;
            next_pass_is_idr = false;
            for (i, nal) in nal_info_vec.into_iter().enumerate() {
                if rebuild_pending {
                    log_post_rebuild_emit(
                        &connection_id,
                        "heartbeat",
                        codec,
                        kind_for_pass,
                        was_idr_flag,
                        nal.nal_bytes.as_ref(),
                    );
                    rebuild_pending = false;
                }
                let frame = build_media_frame(
                    &connection_id,
                    seq,
                    if i == 0 { actual_duration_ns } else { 0 },
                    kind_for_pass,
                    codec,
                    nal.nal_bytes.to_vec(),
                );
                seq += 1;
                if !send_frame_or_stop(
                    &media_sender,
                    &error_tx,
                    &connection_id,
                    frame,
                    &mut stop_rx,
                )
                .await
                {
                    return Ok(());
                }
            }
            last_send_time = now;
            last_emit_wall = Some(now);
            continue;
        }

        // Resolution change detection: only consult on real content
        // frames (we already passed the heartbeat / no-content guard
        // above). `should_recreate_for_resolution` additionally
        // short-circuits (0,0) defensively in case any backend leaks
        // an EmptyImageInfo placeholder past the content_changed flag.
        if let Some((new_w, new_h)) = should_recreate_for_resolution(
            encoder_init_size,
            (shared_frame.width, shared_frame.height),
        ) {
            info!(
                "[MediaProducer:{connection_id}] Frame resolution changed {:?} -> {:?}; \
                 recreating encoder",
                encoder_init_size,
                (new_w, new_h)
            );
            // Update encoder_init_size FIRST so the synthetic
            // DisplayInfo built below carries the new dimensions.
            encoder_init_size = (new_w, new_h);
            if let Err(reason) = preflight_encoder_dimensions(encoder_id, encoder_init_size) {
                video_state.store(VIDEO_STATE_BLOCKED, Ordering::Release);
                report_dimension_blocked(
                    &error_tx,
                    &connection_id,
                    encoder_id,
                    encoder_init_size,
                    reason,
                );
                return Ok(());
            }
            encoder = match create_video_encoder(
                &merged_settings,
                &display_info_for_size(&display_info, encoder_init_size),
            ) {
                Ok(encoder) => encoder,
                Err(error) => {
                    report_prepare_failed(
                        &video_state,
                        &error_tx,
                        &connection_id,
                        encoder_id,
                        encoder_init_size,
                        format!("{error}"),
                    );
                    return Ok(());
                }
            };
            video_state.store(VIDEO_STATE_STREAMING, Ordering::Release);
            replay_bitrate_cap(&mut encoder, current_cap_kbps, &connection_id);
            report_streaming(&error_tx, &connection_id, encoder_id, encoder_init_size);
            next_pass_is_idr = true;
            rebuild_pending = true;
            last_emit_for_throttle = std::time::Instant::now()
                .checked_sub(frame_interval)
                .unwrap_or_else(std::time::Instant::now);
        }

        // fps throttle: skip the frame entirely if our last emit was
        // less than `frame_interval` ago. The shared capture loop
        // produces frames at the OS refresh rate; a 30 fps
        // connection effectively takes every other frame at 60 Hz.
        if now.duration_since(last_emit_for_throttle) < frame_interval {
            continue;
        }

        let nal_info_vec = match encoder.encode(
            shared_frame.as_ref() as &dyn ImageInfo,
            merged_settings.enable_dirty_rect,
        ) {
            Ok(v) => {
                consecutive_encode_failures = 0;
                v
            }
            Err(e) => {
                if record_encode_failure(&mut consecutive_encode_failures) {
                    report_runtime_failed(
                        &video_state,
                        &error_tx,
                        &connection_id,
                        encoder_id,
                        encoder_init_size,
                        format!("encode failed three consecutive times: {e}"),
                    );
                    return Ok(());
                }
                warn!(
                    "[MediaProducer:{connection_id}] transient encode error \
                     ({consecutive_encode_failures}/3): {e}"
                );
                continue;
            }
        };
        // Honour the encoder's native frame-type signal alongside our
        // own `next_pass_is_idr` rebuild marker. With a wider GOP
        // (default 120) the encoder still emits periodic IDRs without
        // any worker-side rebuild — those need to be labelled VideoI
        // so the daemon's paused-write_sample latch can clear on them
        // and host-side keyframe counts align with the browser.
        let kind_for_pass = classify_video_frame_kind(&nal_info_vec, next_pass_is_idr);
        let was_idr_flag = next_pass_is_idr;
        next_pass_is_idr = false;
        // Same dynamic-duration treatment as the heartbeat path:
        // when broadcast lag (or a paused capture) makes the real
        // gap between emits longer than 1/fps, the receiver's RTP
        // timestamp must reflect that or its jitter buffer drifts
        // ahead of wall clock.
        let actual_duration_ns = compute_emit_duration_ns(last_emit_wall, now, frame_duration_ns);
        for (i, nal) in nal_info_vec.into_iter().enumerate() {
            if rebuild_pending {
                log_post_rebuild_emit(
                    &connection_id,
                    "encode",
                    codec,
                    kind_for_pass,
                    was_idr_flag,
                    nal.nal_bytes.as_ref(),
                );
                rebuild_pending = false;
            }
            let frame = build_media_frame(
                &connection_id,
                seq,
                if i == 0 { actual_duration_ns } else { 0 },
                kind_for_pass,
                codec,
                nal.nal_bytes.to_vec(),
            );
            seq += 1;
            if !send_frame_or_stop(
                &media_sender,
                &error_tx,
                &connection_id,
                frame,
                &mut stop_rx,
            )
            .await
            {
                return Ok(());
            }
        }
        last_send_time = now;
        last_emit_wall = Some(now);
        last_emit_for_throttle = now;
    }

    info!("[MediaProducer:{connection_id}] Pipeline exiting (stop_flag observed)");
    Ok(())
}

fn preflight_encoder_dimensions(
    encoder_id: Option<VideoEncoderId>,
    size: (u32, u32),
) -> Result<EncoderCompatibility, EncoderCompatibilityError> {
    let Some(encoder_id) = encoder_id else {
        return Ok(EncoderCompatibility::RuntimeProbeRequired);
    };
    let capability = VideoEncoderCapability::for_id(encoder_id);
    check_encoder_input(Resolution::new(size.0, size.1), &capability.input_support)
}

pub(super) fn record_encode_failure(consecutive_failures: &mut u8) -> bool {
    *consecutive_failures = consecutive_failures.saturating_add(1);
    *consecutive_failures >= 3
}

pub(super) fn report_dimension_blocked(
    error_tx: &mpsc::UnboundedSender<WorkerToService>,
    connection_id: &str,
    encoder_id: Option<VideoEncoderId>,
    size: (u32, u32),
    reason: EncoderCompatibilityError,
) {
    let source_resolution = Resolution::new(size.0, size.1);
    let capabilities = capabilities_for_encoder_names(list_video_encoder());
    let message = format!(
        "video encoder preflight blocked: encoder={encoder_id:?}, source={}x{}, reason={reason:?}",
        size.0, size.1
    );
    warn!("[MediaProducer:{connection_id}] {message}");
    let _ = error_tx.send(WorkerToService::MediaPipelineState(
        MediaPipelineStatePayload {
            connection_id: connection_id.to_string(),
            data: MediaPipelineStateData::blocked_dimensions(
                encoder_id,
                source_resolution,
                compatible_encoders(source_resolution, &capabilities),
                message,
            ),
        },
    ));
}

pub(super) fn report_prepare_failed(
    video_state: &AtomicU8,
    error_tx: &mpsc::UnboundedSender<WorkerToService>,
    connection_id: &str,
    encoder_id: Option<VideoEncoderId>,
    size: (u32, u32),
    message: String,
) {
    video_state.store(VIDEO_STATE_FAILED, Ordering::Release);
    let message = format!(
        "video encoder prepare failed: encoder={encoder_id:?}, source={}x{}: {message}",
        size.0, size.1
    );
    error!("[MediaProducer:{connection_id}] {message}");
    let _ = error_tx.send(WorkerToService::MediaPipelineState(
        MediaPipelineStatePayload {
            connection_id: connection_id.to_string(),
            data: MediaPipelineStateData {
                phase: MediaPipelinePhase::Failed,
                encoder: encoder_id,
                source_resolution: Some(Resolution::new(size.0, size.1)),
                compatible_encoders: Vec::new(),
                reason_code: Some(DeskErrorCode::VIDEO_ENCODER_PREPARE_FAILED),
                message: Some(message),
            },
        },
    ));
}

pub(super) fn report_runtime_failed(
    video_state: &AtomicU8,
    error_tx: &mpsc::UnboundedSender<WorkerToService>,
    connection_id: &str,
    encoder_id: Option<VideoEncoderId>,
    size: (u32, u32),
    message: String,
) {
    video_state.store(VIDEO_STATE_FAILED, Ordering::Release);
    error!("[MediaProducer:{connection_id}] {message}");
    let _ = error_tx.send(WorkerToService::MediaPipelineState(
        MediaPipelineStatePayload {
            connection_id: connection_id.to_string(),
            data: MediaPipelineStateData {
                phase: MediaPipelinePhase::Failed,
                encoder: encoder_id,
                source_resolution: Some(Resolution::new(size.0, size.1)),
                compatible_encoders: Vec::new(),
                reason_code: Some(DeskErrorCode::VIDEO_PIPELINE_RUNTIME_FAILED),
                message: Some(message),
            },
        },
    ));
}

fn report_streaming(
    error_tx: &mpsc::UnboundedSender<WorkerToService>,
    connection_id: &str,
    encoder_id: Option<VideoEncoderId>,
    size: (u32, u32),
) {
    let _ = error_tx.send(WorkerToService::MediaPipelineState(
        MediaPipelineStatePayload {
            connection_id: connection_id.to_string(),
            data: MediaPipelineStateData {
                phase: MediaPipelinePhase::Streaming,
                encoder: encoder_id,
                source_resolution: Some(Resolution::new(size.0, size.1)),
                compatible_encoders: Vec::new(),
                reason_code: None,
                message: None,
            },
        },
    ));
}
