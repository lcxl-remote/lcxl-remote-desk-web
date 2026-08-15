use super::*;

/// Inner async loop for a connection-scoped audio runner:
/// 5 ms ticker drives an inner buffer-drain loop
/// that pulls 20 ms Opus packets out of the encoder and ships each one
/// as a `MediaFrame { Audio }` to the daemon. The daemon's
/// `write_video_frame` already routes `MediaFrameKind::Audio` to the
/// per-PC `audio_track`, so no daemon-side change is needed for audio
/// frames themselves to reach the browser.
///
/// **Audio codec is locked to Opus** for now — the only audio encoder
/// the capture-engine factory ships.
///
/// Failures during capture init / start / encode propagate up as
/// `Err(String)` and the spawning thread logs them at warn level — a
/// degraded video-only stream is preferable to the connection
/// crashing.
pub(super) async fn audio_pipeline_loop(
    base_settings: DeskSettings,
    payload: StartMediaPayload,
    media_sender: Arc<dyn MediaSender>,
    error_tx: mpsc::UnboundedSender<WorkerToService>,
    stop_flag: Arc<AtomicBool>,
) -> Result<(), String> {
    let connection_id = payload.connection_id.clone();
    let audio = payload
        .audio
        .as_ref()
        .ok_or_else(|| "audio pipeline started without audio settings".to_string())?;
    if !matches!(audio.codec, MediaCodec::Opus) {
        warn!(
            "[MediaProducer:{connection_id}] Requested audio codec {:?} is not Opus; \
             worker only ships Opus today",
            audio.codec,
        );
        return Err("unsupported audio codec".to_string());
    }

    let mut effective_settings = base_settings;
    effective_settings.audio_capture = Some(audio.pipeline.audio_capture.clone());
    effective_settings.audio_device = Some(audio.pipeline.audio_device.clone());
    effective_settings.audio_encoder = Some(audio.pipeline.audio_encoder.setting_name().into());

    info!("[MediaProducer:{connection_id}] Starting audio pipeline (Opus)");

    let mut capture = create_audio_capture(&effective_settings).map_err(|e| format!("{e}"))?;
    let wave_format = capture.start().map_err(|e| format!("{e}"))?;
    let mut encoder =
        create_audio_encoder(&effective_settings, wave_format).map_err(|e| format!("{e}"))?;

    // A Stop can arrive while capture/encoder construction is blocking. Do
    // not publish `Active` after the slot has already selected `Off` as its
    // terminal; the thread wrapper will publish the matching Off event.
    if stop_flag.load(Ordering::Acquire) {
        return Ok(());
    }

    let _ = error_tx.send(WorkerToService::AudioPipelineStateChanged(
        AudioPipelineStateChangedPayload {
            connection_id: connection_id.clone(),
            connection_epoch: payload.connection_epoch.clone(),
            audio_generation: payload.audio_generation,
            phase: AudioPipelinePhase::Active,
            resolved_audio_device_id: None,
            error_code: None,
        },
    ));

    // 5 ms outer tick + inner drain loop sets the pacing —
    // capture buffers fill at the OS audio cadence (typically 10 ms),
    // and at 5 ms ticks we drain everything sitting in the buffer
    // before sleeping again. Opus encoded packets carry 20 ms of audio
    // each by capture-engine convention.
    let mut ticker = tokio::time::interval(Duration::from_millis(5));
    const AUDIO_FRAME_DURATION: Duration = Duration::from_millis(20);
    let audio_duration_ns = AUDIO_FRAME_DURATION.as_nanos().min(u64::MAX as u128) as u64;
    let mut seq: u64 = 0;

    while !stop_flag.load(Ordering::Relaxed) {
        ticker.tick().await;
        if stop_flag.load(Ordering::Relaxed) {
            break;
        }

        // Drain whatever the capture has buffered. The inner loop
        // exits on Empty (encoded buffer length 0) so we get back to
        // the ticker and yield to the rest of the runtime.
        loop {
            if stop_flag.load(Ordering::Relaxed) {
                break;
            }
            let buffer = match capture.get_buffer() {
                Ok(b) => b,
                Err(desk_capture_engine::error::CaptureError::CustomError(err))
                    if err.error_code == desk_utils::error::DeskErrorCode::ACTION_NEED_RETRY =>
                {
                    // Capture stream went away (device unplug, format
                    // change, sleep/resume). Recreate the capture and
                    // continue from the next tick.
                    warn!(
                        "[MediaProducer:{connection_id}] audio capture needs retry — \
                         recreating capture"
                    );
                    capture = match create_audio_capture(&effective_settings) {
                        Ok(c) => c,
                        Err(e) => {
                            warn!(
                                "[MediaProducer:{connection_id}] audio capture rebuild failed: \
                                 {e}; audio pipeline exiting"
                            );
                            return Ok(());
                        }
                    };
                    if let Err(e) = capture.start() {
                        warn!(
                            "[MediaProducer:{connection_id}] audio capture restart failed: \
                             {e}; audio pipeline exiting"
                        );
                        return Ok(());
                    }
                    break;
                }
                Err(e) => {
                    warn!(
                        "[MediaProducer:{connection_id}] audio get_buffer error: {e}; \
                         skipping this tick"
                    );
                    break;
                }
            };

            let encoded = match encoder.encode(buffer.as_ref()) {
                Ok(e) => e,
                Err(e) => {
                    warn!(
                        "[MediaProducer:{connection_id}] audio encode error: {e}; \
                         skipping packet"
                    );
                    break;
                }
            };
            // Empty buffer = capture had nothing this tick — go back
            // to the ticker without sending.
            if encoded.data.is_empty() {
                break;
            }
            let frame = build_media_frame(
                &connection_id,
                &payload.connection_epoch,
                payload.audio_generation,
                seq,
                audio_duration_ns,
                MediaFrameKind::Audio,
                MediaCodec::Opus,
                encoded.data,
            );
            seq += 1;
            if !send_frame(&media_sender, &error_tx, &connection_id, frame).await {
                return Ok(());
            }
        }
    }

    info!("[MediaProducer:{connection_id}] Audio pipeline exiting (stop_flag observed)");
    Ok(())
}
