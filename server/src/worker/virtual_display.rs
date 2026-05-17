//! Worker-side state for the virtual display feature.
//!
//! The daemon owns the `SwDevice` handle (see
//! [`crate::daemon::virtual_display`]); the worker owns the runtime
//! pieces that have to live in the interactive user session:
//!
//! - `attached_display` tracks the `\\.\DISPLAYn` device name the
//!   daemon currently has wired up.
//! - `original_start_payload` keeps each connection's StartMedia
//!   exactly as the daemon issued it (preserves the user's preferred
//!   physical capture target).
//! - `active_start_payload` is what we actually handed the producer —
//!   it may have `video_device` overridden when attached.
//!
//! Attach / Detach IPC mutates `attached_display` and emits a
//! `Stop+Start` per active connection so the capture pipeline rebuilds
//! against the new target. SetVirtualDisplayMode IPC drives the
//! [`VirtualDisplayController`] (running on a blocking task because
//! the Windows backend has to wait on `ChangeDisplaySettingsExW` /
//! the driver named pipe).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use desk_ipc_protocol::message::{
    SetVirtualDisplayModePayload, StartMediaPayload, VirtualDisplayAttachOutcome,
    VirtualDisplayModeData, VirtualDisplayModeOutcome, VirtualDisplayModeResponsePayload,
    WorkerToService,
};
use desk_virtual_display::{VirtualDisplayController, VirtualDisplayError, VirtualDisplayMode};

/// Worker-side virtual display state. All mutations happen from the
/// main message loop, so no synchronisation is needed beyond
/// `&mut self`.
#[derive(Default)]
pub struct VirtualDisplayState {
    /// `\\.\DISPLAYn` device name pushed by the daemon's
    /// `AttachVirtualDisplay`. `None` ⇒ capture targets the physical
    /// display the user originally selected.
    pub attached_display: Option<String>,
    /// Original StartMedia exactly as it arrived from the daemon
    /// (preserving the user's preferred physical capture target).
    /// Survives Attach/Detach swaps so a Detach can rebuild capture
    /// against the original target without the daemon re-issuing
    /// StartMedia from scratch.
    pub original_start_payload: HashMap<String, StartMediaPayload>,
    /// StartMedia actually handed to the producer — same as `original`
    /// except `video_device` is overridden to the attached display
    /// name when attached.
    pub active_start_payload: HashMap<String, StartMediaPayload>,
}

impl VirtualDisplayState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build the active payload from `original`, applying the attached
    /// display override if any.
    fn make_active(&self, original: &StartMediaPayload) -> StartMediaPayload {
        match &self.attached_display {
            Some(d) => StartMediaPayload {
                video_device: Some(d.clone()),
                ..original.clone()
            },
            None => original.clone(),
        }
    }

    /// Record an inbound StartMedia: cache `original`, return the
    /// payload that should be forwarded to the producer (already
    /// adjusted for the attached display, if any).
    pub fn record_start(&mut self, payload: StartMediaPayload) -> StartMediaPayload {
        self.original_start_payload
            .insert(payload.connection_id.clone(), payload.clone());
        let active = self.make_active(&payload);
        self.active_start_payload
            .insert(active.connection_id.clone(), active.clone());
        active
    }

    /// Record an inbound StopMedia: drop both caches.
    pub fn record_stop(&mut self, connection_id: &str) {
        self.original_start_payload.remove(connection_id);
        self.active_start_payload.remove(connection_id);
    }

    /// Apply a new attached-display state and re-derive `active`
    /// payloads. Returns the list of (connection_id, active_payload)
    /// the caller should drive Stop+Start against the producer to
    /// swap capture targets.
    pub fn rebuild_active_for_attach(&mut self, display_name: Option<String>) -> Vec<RestartStep> {
        self.attached_display = display_name;
        let originals: Vec<StartMediaPayload> =
            self.original_start_payload.values().cloned().collect();
        self.active_start_payload.clear();
        originals
            .into_iter()
            .map(|orig| {
                let active = self.make_active(&orig);
                self.active_start_payload
                    .insert(active.connection_id.clone(), active.clone());
                RestartStep {
                    connection_id: orig.connection_id.clone(),
                    active,
                }
            })
            .collect()
    }
}

/// A (stop, start) instruction the caller should drive against the
/// media producer to swap a single connection's capture target.
pub struct RestartStep {
    pub connection_id: String,
    pub active: StartMediaPayload,
}

/// Exponential-backoff schedule for the attach resolver. Six attempts,
/// `250 / 500 / 1000 / 2000 / 4000 / 8000` ms between them (~15.75 s
/// total wall time). Sized to comfortably cover the worst-case IDD
/// driver bring-up + GDI enumeration race observed on a cold-installed
/// host; if six rounds still fail the problem is structural (driver
/// crashed, monitor never enumerated) and further retries would only
/// hide the failure.
pub const ATTACH_BACKOFF_SCHEDULE_MS: [u64; 6] = [250, 500, 1000, 2000, 4000, 8000];

/// Resolve a PnP instance id to a GDI `\\.\DISPLAYn` with bounded
/// exponential-backoff retries. Pure function — no side effects on
/// worker state — so the caller (worker session loop) wires the result
/// into `VirtualDisplayState` and the media producer.
///
/// The `resolver` and `sleeper` closures are injectable so unit tests
/// drive arbitrary retry sequences without sleeping real wall time.
/// Production wires `resolver = desk_virtual_display::resolve_display_name`
/// and `sleeper = tokio::time::sleep`.
///
/// Returns:
/// - [`VirtualDisplayAttachOutcome::Attached`] with the resolved
///   display name on the first successful resolver call.
/// - [`VirtualDisplayAttachOutcome::Failed`] carrying the last
///   resolver error message if every retry slot was exhausted.
pub async fn resolve_attach_with_backoff<R, S, Fut>(
    instance_id: &str,
    mut resolver: R,
    mut sleeper: S,
) -> VirtualDisplayAttachOutcome
where
    R: FnMut(&str) -> Result<String, VirtualDisplayError>,
    S: FnMut(Duration) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let schedule = ATTACH_BACKOFF_SCHEDULE_MS;
    let mut last_err: String = String::new();
    for (attempt, &sleep_ms) in schedule.iter().enumerate() {
        match resolver(instance_id) {
            Ok(display_name) => return VirtualDisplayAttachOutcome::Attached(display_name),
            Err(e) => {
                last_err = e.to_string();
                let is_last_attempt = attempt + 1 == schedule.len();
                if is_last_attempt {
                    // No sleep after the final attempt — exit the loop
                    // and fall through to the Failed return below.
                    break;
                }
                tracing::debug!(
                    virtual_display.instance_id = %instance_id,
                    virtual_display.attempt = attempt + 1,
                    virtual_display.backoff_ms = sleep_ms,
                    "resolve_attach_with_backoff: attempt failed: {e}; will retry",
                );
                sleeper(Duration::from_millis(sleep_ms)).await;
            }
        }
    }
    VirtualDisplayAttachOutcome::Failed(format!(
        "exhausted {} retries resolving instance {instance_id}: {last_err}",
        schedule.len(),
    ))
}

/// Drive `controller.set_mode` on a blocking thread (the Windows
/// backend has to wait on synchronous Win32 IO) and pack the result
/// into the `WorkerToService::VirtualDisplayMode` reply the daemon
/// will ferry back to the originating browser.
pub async fn run_set_mode(
    controller: Arc<dyn VirtualDisplayController>,
    attached_display: Option<String>,
    payload: SetVirtualDisplayModePayload,
) -> WorkerToService {
    let display = match attached_display {
        Some(d) => d,
        None => {
            return WorkerToService::VirtualDisplayMode(VirtualDisplayModeResponsePayload {
                request_id: payload.request_id,
                connection_id: payload.connection_id,
                outcome: VirtualDisplayModeOutcome::Failed(
                    "virtual display not attached".to_string(),
                ),
            });
        }
    };
    let mode = VirtualDisplayMode {
        width: payload.width,
        height: payload.height,
        refresh_hz: payload.refresh_hz,
    };
    let join_result =
        tokio::task::spawn_blocking(move || controller.set_mode(&display, mode)).await;
    let result =
        join_result.unwrap_or_else(|e| Err(VirtualDisplayError::PipeIo(format!("join: {e}"))));
    let outcome = match result {
        Ok(m) => VirtualDisplayModeOutcome::Applied(VirtualDisplayModeData {
            width: m.width,
            height: m.height,
            refresh_hz: m.refresh_hz,
        }),
        Err(e) => VirtualDisplayModeOutcome::Failed(e.to_string()),
    };
    WorkerToService::VirtualDisplayMode(VirtualDisplayModeResponsePayload {
        request_id: payload.request_id,
        connection_id: payload.connection_id,
        outcome,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_ipc_protocol::message::MediaCodec;
    use desk_virtual_display::VirtualDisplayController;
    use std::sync::Mutex;

    fn make_payload(connection_id: &str, video_device: Option<&str>) -> StartMediaPayload {
        StartMediaPayload {
            connection_id: connection_id.to_string(),
            video_codec: MediaCodec::H264,
            audio_codec: MediaCodec::Opus,
            video_device: video_device.map(|s| s.to_string()),
            audio_device: None,
            fps: 60,
            bitrate_kbps: 4_000,
            quality: 0,
            start_video: true,
            start_audio: true,
            image_capture: None,
            enable_dirty_rect: None,
        }
    }

    #[test]
    fn record_start_caches_original_and_overrides_active_when_attached() {
        let mut state = VirtualDisplayState::new();
        state.attached_display = Some("\\\\.\\DISPLAY9".to_string());
        let original = make_payload("conn-1", Some("\\\\.\\DISPLAY1"));
        let active = state.record_start(original.clone());
        // original is preserved exactly.
        assert_eq!(
            state
                .original_start_payload
                .get("conn-1")
                .unwrap()
                .video_device
                .as_deref(),
            Some("\\\\.\\DISPLAY1"),
        );
        // active is overridden.
        assert_eq!(active.video_device.as_deref(), Some("\\\\.\\DISPLAY9"));
        assert_eq!(
            state
                .active_start_payload
                .get("conn-1")
                .unwrap()
                .video_device
                .as_deref(),
            Some("\\\\.\\DISPLAY9"),
        );
    }

    #[test]
    fn record_start_caches_original_unchanged_when_not_attached() {
        let mut state = VirtualDisplayState::new();
        let original = make_payload("conn-1", Some("\\\\.\\DISPLAY1"));
        let active = state.record_start(original.clone());
        assert_eq!(active.video_device.as_deref(), Some("\\\\.\\DISPLAY1"));
        assert_eq!(
            state
                .original_start_payload
                .get("conn-1")
                .unwrap()
                .video_device
                .as_deref(),
            Some("\\\\.\\DISPLAY1"),
        );
    }

    #[test]
    fn record_stop_clears_both_caches() {
        let mut state = VirtualDisplayState::new();
        state.record_start(make_payload("conn-1", Some("\\\\.\\DISPLAY1")));
        state.record_stop("conn-1");
        assert!(state.original_start_payload.is_empty());
        assert!(state.active_start_payload.is_empty());
    }

    #[test]
    fn rebuild_active_for_attach_emits_restart_steps_for_each_connection() {
        let mut state = VirtualDisplayState::new();
        state.record_start(make_payload("conn-1", Some("\\\\.\\DISPLAY1")));
        state.record_start(make_payload("conn-2", Some("\\\\.\\DISPLAY1")));
        let steps = state.rebuild_active_for_attach(Some("\\\\.\\DISPLAY9".to_string()));
        assert_eq!(steps.len(), 2);
        for step in &steps {
            assert_eq!(step.active.video_device.as_deref(), Some("\\\\.\\DISPLAY9"));
        }
        // Active cache now reflects the override.
        for active in state.active_start_payload.values() {
            assert_eq!(active.video_device.as_deref(), Some("\\\\.\\DISPLAY9"));
        }
        // Original is untouched.
        for original in state.original_start_payload.values() {
            assert_eq!(original.video_device.as_deref(), Some("\\\\.\\DISPLAY1"));
        }
    }

    #[test]
    fn rebuild_active_for_detach_restores_original_video_device() {
        let mut state = VirtualDisplayState::new();
        state.attached_display = Some("\\\\.\\DISPLAY9".to_string());
        state.record_start(make_payload("conn-1", Some("\\\\.\\DISPLAY1")));
        let steps = state.rebuild_active_for_attach(None);
        assert_eq!(steps.len(), 1);
        // Detach restores the original physical device.
        assert_eq!(
            steps[0].active.video_device.as_deref(),
            Some("\\\\.\\DISPLAY1")
        );
        assert!(state.attached_display.is_none());
    }

    struct MockController {
        applied_mode: Mutex<Option<VirtualDisplayMode>>,
        result: fn(VirtualDisplayMode) -> Result<VirtualDisplayMode, VirtualDisplayError>,
    }

    impl VirtualDisplayController for MockController {
        fn set_mode(
            &self,
            _: &str,
            mode: VirtualDisplayMode,
        ) -> Result<VirtualDisplayMode, VirtualDisplayError> {
            *self.applied_mode.lock().unwrap() = Some(mode);
            (self.result)(mode)
        }
    }

    /// `tokio::time::sleep` is awkward to mock at the type level. The
    /// existing tests wrap the injected sleeper around a counter and
    /// assert on the recorded `Duration`s instead, so we never spend
    /// real wall time waiting for the 250 / 500 / ... backoff schedule.
    fn make_recording_sleeper() -> (
        std::sync::Arc<Mutex<Vec<Duration>>>,
        impl FnMut(Duration) -> std::future::Ready<()>,
    ) {
        let log: std::sync::Arc<Mutex<Vec<Duration>>> = std::sync::Arc::new(Mutex::new(Vec::new()));
        let log_clone = std::sync::Arc::clone(&log);
        let sleeper = move |d: Duration| {
            log_clone.lock().unwrap().push(d);
            std::future::ready(())
        };
        (log, sleeper)
    }

    #[tokio::test]
    async fn resolve_attach_with_backoff_returns_attached_on_first_success() {
        let calls = std::sync::Arc::new(Mutex::new(0u32));
        let calls_clone = std::sync::Arc::clone(&calls);
        let resolver = move |_: &str| {
            *calls_clone.lock().unwrap() += 1;
            Ok(r"\\.\DISPLAY4".to_string())
        };
        let (sleep_log, sleeper) = make_recording_sleeper();
        let outcome = resolve_attach_with_backoff(
            "SWD\\LcxlVirtualDisplay\\LcxlVirtualDisplay",
            resolver,
            sleeper,
        )
        .await;
        assert!(matches!(
            outcome,
            VirtualDisplayAttachOutcome::Attached(ref n) if n == r"\\.\DISPLAY4"
        ));
        assert_eq!(*calls.lock().unwrap(), 1, "first success must short-circuit");
        assert!(
            sleep_log.lock().unwrap().is_empty(),
            "no backoff sleep needed on first success",
        );
    }

    #[tokio::test]
    async fn resolve_attach_with_backoff_retries_until_success() {
        // First two attempts fail, third succeeds.
        let calls = std::sync::Arc::new(Mutex::new(0u32));
        let calls_clone = std::sync::Arc::clone(&calls);
        let resolver = move |_: &str| -> Result<String, VirtualDisplayError> {
            let mut n = calls_clone.lock().unwrap();
            *n += 1;
            if *n < 3 {
                Err(VirtualDisplayError::DeviceCreate(format!(
                    "transient #{}",
                    *n
                )))
            } else {
                Ok(r"\\.\DISPLAY4".to_string())
            }
        };
        let (sleep_log, sleeper) = make_recording_sleeper();
        let outcome = resolve_attach_with_backoff(
            "SWD\\LcxlVirtualDisplay\\LcxlVirtualDisplay",
            resolver,
            sleeper,
        )
        .await;
        assert!(matches!(
            outcome,
            VirtualDisplayAttachOutcome::Attached(ref n) if n == r"\\.\DISPLAY4"
        ));
        assert_eq!(*calls.lock().unwrap(), 3, "must retry until success");
        // Two sleeps between the three attempts: 250, 500.
        let log = sleep_log.lock().unwrap();
        assert_eq!(*log, vec![Duration::from_millis(250), Duration::from_millis(500)]);
    }

    #[tokio::test]
    async fn resolve_attach_with_backoff_returns_failed_after_max_retries() {
        let calls = std::sync::Arc::new(Mutex::new(0u32));
        let calls_clone = std::sync::Arc::clone(&calls);
        let resolver = move |_: &str| -> Result<String, VirtualDisplayError> {
            *calls_clone.lock().unwrap() += 1;
            Err(VirtualDisplayError::DeviceCreate("permanent".to_string()))
        };
        let (sleep_log, sleeper) = make_recording_sleeper();
        let outcome = resolve_attach_with_backoff(
            "SWD\\LcxlVirtualDisplay\\LcxlVirtualDisplay",
            resolver,
            sleeper,
        )
        .await;
        match outcome {
            VirtualDisplayAttachOutcome::Failed(msg) => {
                assert!(
                    msg.contains("exhausted 6 retries"),
                    "expected exhaustion summary, got {msg}",
                );
                assert!(msg.contains("permanent"), "expected last error in message, got {msg}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        assert_eq!(
            *calls.lock().unwrap(),
            ATTACH_BACKOFF_SCHEDULE_MS.len() as u32,
            "every retry slot must be consumed",
        );
        // 5 sleeps between 6 attempts; the trailing 8000 ms slot is
        // skipped because it would just delay returning Failed.
        let log = sleep_log.lock().unwrap();
        assert_eq!(
            *log,
            vec![
                Duration::from_millis(250),
                Duration::from_millis(500),
                Duration::from_millis(1_000),
                Duration::from_millis(2_000),
                Duration::from_millis(4_000),
            ],
        );
    }

    #[tokio::test]
    async fn run_set_mode_returns_failed_when_not_attached() {
        let controller: Arc<dyn VirtualDisplayController> = Arc::new(MockController {
            applied_mode: Mutex::new(None),
            result: |_| panic!("controller must not be invoked when not attached"),
        });
        let payload = SetVirtualDisplayModePayload {
            request_id: "r".to_string(),
            connection_id: "c".to_string(),
            width: 1280,
            height: 720,
            refresh_hz: 60,
        };
        let response = run_set_mode(controller, None, payload).await;
        match response {
            WorkerToService::VirtualDisplayMode(p) => match p.outcome {
                VirtualDisplayModeOutcome::Failed(reason) => {
                    assert_eq!(reason, "virtual display not attached");
                }
                other => panic!("unexpected outcome: {other:?}"),
            },
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_set_mode_returns_applied_on_success() {
        let controller: Arc<dyn VirtualDisplayController> = Arc::new(MockController {
            applied_mode: Mutex::new(None),
            result: |m| Ok(m),
        });
        let payload = SetVirtualDisplayModePayload {
            request_id: "r".to_string(),
            connection_id: "c".to_string(),
            width: 1920,
            height: 1080,
            refresh_hz: 60,
        };
        let response = run_set_mode(controller, Some("\\\\.\\DISPLAY9".to_string()), payload).await;
        match response {
            WorkerToService::VirtualDisplayMode(p) => match p.outcome {
                VirtualDisplayModeOutcome::Applied(m) => {
                    assert_eq!(m.width, 1920);
                    assert_eq!(m.height, 1080);
                    assert_eq!(m.refresh_hz, 60);
                }
                other => panic!("unexpected outcome: {other:?}"),
            },
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_set_mode_returns_failed_on_controller_error() {
        let controller: Arc<dyn VirtualDisplayController> = Arc::new(MockController {
            applied_mode: Mutex::new(None),
            result: |_| {
                Err(VirtualDisplayError::PipeIo(
                    "driver not available".to_string(),
                ))
            },
        });
        let payload = SetVirtualDisplayModePayload {
            request_id: "r".to_string(),
            connection_id: "c".to_string(),
            width: 1280,
            height: 720,
            refresh_hz: 60,
        };
        let response = run_set_mode(controller, Some("\\\\.\\DISPLAY9".to_string()), payload).await;
        match response {
            WorkerToService::VirtualDisplayMode(p) => match p.outcome {
                VirtualDisplayModeOutcome::Failed(reason) => {
                    assert!(
                        reason.contains("driver not available"),
                        "expected driver-not-available message, got {reason}",
                    );
                }
                other => panic!("unexpected outcome: {other:?}"),
            },
            other => panic!("unexpected: {other:?}"),
        }
    }
}
