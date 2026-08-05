use super::*;

/// Spawn the dedicated thread that owns one connection's video
/// capture + encoder. Uses a current-thread Tokio runtime inside the
/// thread so `media_sender.send_frame(...).await` can run without
/// polluting the outer runtime with COM-bound state.
pub(super) fn spawn_video_pipeline_thread(
    base_settings: DeskSettings,
    payload: StartMediaPayload,
    media_sender: Arc<dyn MediaSender>,
    error_tx: mpsc::UnboundedSender<WorkerToService>,
    stop_flag: Arc<AtomicBool>,
    stop_rx: watch::Receiver<bool>,
    keyframe_requested: Arc<AtomicBool>,
    settings_rx: mpsc::UnboundedReceiver<UpdateMediaSettingsPayload>,
    capture_registry: Arc<SharedCaptureRegistry>,
    capture_keys: Arc<StdMutex<HashMap<String, CaptureKeyRecord>>>,
    generation: u64,
    geometry_update_handler: Option<Arc<GeometryUpdateHandler>>,
) -> thread::JoinHandle<()> {
    let connection_id = payload.connection_id.clone();
    let thread_name = format!("media-video-{}", &connection_id);
    thread::Builder::new()
        .name(thread_name)
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    error!(
                        "[MediaProducer] Failed to build video runtime for {connection_id}: {e}; \
                         pipeline thread exits before first frame"
                    );
                    return;
                }
            };
            let local = tokio::task::LocalSet::new();
            runtime.block_on(local.run_until(async move {
                if let Err(e) = video_pipeline_loop(
                    base_settings,
                    payload,
                    media_sender,
                    error_tx,
                    stop_flag,
                    stop_rx,
                    keyframe_requested,
                    settings_rx,
                    capture_registry,
                    capture_keys,
                    generation,
                    geometry_update_handler,
                )
                .await
                {
                    error!(
                        "[MediaProducer] Video pipeline for {connection_id} exited with error: {e}"
                    );
                }
            }));
        })
        .expect("spawn media video pipeline thread")
}

/// Spawn the dedicated thread that owns one connection's audio
/// capture + Opus encoder. Same threading rationale as
/// [`spawn_video_pipeline_thread`] — WASAPI / PipeWire / SCKit handles
/// are system-thread-bound, so audio gets its own thread + runtime.
/// Errors during construction or capture are logged but never bring
/// down the worker; the daemon already tolerates a video-only stream
/// when the worker has no audio device available.
pub(super) fn spawn_audio_pipeline_thread(
    base_settings: DeskSettings,
    payload: StartMediaPayload,
    media_sender: Arc<dyn MediaSender>,
    error_tx: mpsc::UnboundedSender<WorkerToService>,
    stop_flag: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    let connection_id = payload.connection_id.clone();
    let thread_name = format!("media-audio-{}", &connection_id);
    thread::Builder::new()
        .name(thread_name)
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    error!(
                        "[MediaProducer] Failed to build audio runtime for {connection_id}: {e}; \
                         audio pipeline thread exits before first sample"
                    );
                    return;
                }
            };
            let local = tokio::task::LocalSet::new();
            runtime.block_on(local.run_until(async move {
                if let Err(e) =
                    audio_pipeline_loop(base_settings, payload, media_sender, error_tx, stop_flag)
                        .await
                {
                    // Audio failures degrade the stream to video-only
                    // but must not crash the connection; logged so the
                    // operator can investigate.
                    warn!("[MediaProducer] Audio pipeline for {connection_id} exited: {e}");
                }
            }));
        })
        .expect("spawn media audio pipeline thread")
}
