//! A single native owner keeps an authorized PipeWire consumer between reads.
use super::*;
use crate::worker::agent::computer_use_broker::ScreenCapturePermit;
use desk_capture_engine::image_capture::pipewire_capture::CaptureContinuity;
use std::sync::{
    Mutex, Weak,
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc,
};

static THREADS: AtomicUsize = AtomicUsize::new(0);
const MAX_THREADS: usize = 8;
const IDLE_LIMIT: Duration = Duration::from_secs(30);

struct ThreadSlot;
impl Drop for ThreadSlot {
    fn drop(&mut self) {
        THREADS.fetch_sub(1, Ordering::AcqRel);
    }
}

pub(crate) struct Snapshot {
    pub(crate) output: ScreenCaptureOutput,
    pub(crate) continuity: CaptureContinuity,
    pub(crate) received_at: Instant,
}

struct Work<P> {
    deadline: Instant,
    reply: tokio::sync::oneshot::Sender<Result<Snapshot, AgentError>>,
    // Admission stays owned until native encoding actually finishes.
    _permit: P,
}

type Request = Work<ScreenCapturePermit>;

impl<P> Work<P> {
    fn is_current(&self) -> bool {
        !self.reply.is_closed() && Instant::now() < self.deadline
    }
}

pub(crate) struct Service {
    identity: String,
    generation: u64,
    display: String,
    session: Arc<dyn desk_wayland_portal::LivePortalSession>,
    stop: Arc<AtomicBool>,
    sender: mpsc::SyncSender<Request>,
}

impl Drop for Service {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

impl Service {
    pub(crate) fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }

    fn matches(
        &self,
        identity: &str,
        generation: u64,
        settings: &DeskSettings,
        session: &Arc<dyn desk_wayland_portal::LivePortalSession>,
    ) -> bool {
        !self.stop.load(Ordering::Acquire)
            && self.identity == identity
            && self.generation == generation
            && self.display == settings.video_device_name
            && Arc::ptr_eq(&self.session, session)
            && !session.closure_token().is_cancelled()
    }

    fn start(
        broker: Weak<ComputerUseBroker>,
        identity: String,
        generation: u64,
        settings: DeskSettings,
        session: Arc<dyn desk_wayland_portal::LivePortalSession>,
    ) -> Result<Arc<Self>, AgentError> {
        THREADS
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < MAX_THREADS).then_some(count + 1)
            })
            .map_err(|_| internal("Wayland snapshot thread capacity exhausted"))?;
        let slot = ThreadSlot;
        let (sender, receiver) = mpsc::sync_channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let service = Arc::new(Self {
            identity: identity.clone(),
            generation,
            display: settings.video_device_name.clone(),
            session: session.clone(),
            stop: stop.clone(),
            sender,
        });
        std::thread::Builder::new()
            .name("wayland-snapshots".into())
            .spawn(move || {
                let _slot = slot;
                struct Stopped(Arc<AtomicBool>);
                impl Drop for Stopped {
                    fn drop(&mut self) {
                        self.0.store(true, Ordering::Release);
                    }
                }
                let _stopped = Stopped(stop.clone());
                let mut capture = match WaylandPortalImageCapture::new(&settings, session.clone()) {
                    Ok(capture) => capture,
                    Err(_) => return,
                };
                let mut latest: Option<(Box<dyn ImageInfo + Send + Sync>, CaptureContinuity)> =
                    None;
                let mut activity = Instant::now();
                loop {
                    if stop.load(Ordering::Acquire)
                        || session.closure_token().is_cancelled()
                        || activity.elapsed() >= IDLE_LIMIT
                        || !broker.upgrade().is_some_and(|broker| {
                            broker
                                .linux_desktop_identity()
                                .is_some_and(|current| current.binding() == identity)
                        })
                    {
                        break;
                    }
                    let request = match receiver.recv_timeout(Duration::from_millis(100)) {
                        Ok(request) => request,
                        Err(mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    };
                    activity = Instant::now();
                    if !request.is_current() {
                        continue;
                    }
                    let result = read(
                        &mut capture,
                        &mut latest,
                        &settings.video_device_name,
                        generation,
                        &session,
                        &stop,
                        &request,
                    );
                    let terminal = result.is_err();
                    let _ = request.reply.send(result);
                    if terminal {
                        break;
                    }
                }
            })
            .map_err(|_| internal("Wayland snapshot thread could not start"))?;
        Ok(service)
    }
}

pub(crate) async fn snapshot(
    slot: &Mutex<Option<Arc<Service>>>,
    broker: &Arc<ComputerUseBroker>,
    identity: String,
    generation: u64,
    settings: DeskSettings,
    session: Arc<dyn desk_wayland_portal::LivePortalSession>,
    permit: ScreenCapturePermit,
) -> Result<Snapshot, AgentError> {
    let service = {
        let mut slot = slot
            .lock()
            .map_err(|_| internal("Wayland snapshot state unavailable"))?;
        if !slot
            .as_ref()
            .is_some_and(|service| service.matches(&identity, generation, &settings, &session))
        {
            if let Some(previous) = slot.take() {
                previous.stop();
            }
            *slot = Some(Service::start(
                Arc::downgrade(broker),
                identity,
                generation,
                settings,
                session,
            )?);
        }
        slot.as_ref().unwrap().clone()
    };
    let (reply, receiver) = tokio::sync::oneshot::channel();
    service
        .sender
        .try_send(Request {
            deadline: Instant::now() + Duration::from_secs(3),
            reply,
            _permit: permit,
        })
        .map_err(|_| internal("Wayland snapshot queue unavailable"))?;
    tokio::time::timeout(Duration::from_secs(4), receiver)
        .await
        .map_err(|_| internal("Wayland snapshot request timed out"))?
        .map_err(|_| internal("Wayland snapshot consumer stopped"))?
}

fn read(
    capture: &mut WaylandPortalImageCapture,
    latest: &mut Option<(Box<dyn ImageInfo + Send + Sync>, CaptureContinuity)>,
    display: &str,
    generation: u64,
    session: &Arc<dyn desk_wayland_portal::LivePortalSession>,
    stop: &AtomicBool,
    request: &Request,
) -> Result<Snapshot, AgentError> {
    loop {
        if stop.load(Ordering::Acquire)
            || session.closure_token().is_cancelled()
            || !request.is_current()
        {
            return Err(internal(
                "Wayland snapshot ended before a usable frame was returned",
            ));
        }
        match capture.capture(CaptureRequest {
            cursor_mode: CursorCaptureMode::RenderInFrame,
        }) {
            Ok(frame) => {
                let continuity = capture
                    .continuity()
                    .ok_or_else(|| internal("capture continuity missing"))?;
                *latest = Some((frame.image, continuity));
            }
            Err(error)
                if error.to_error_code() == desk_utils::error::DeskErrorCode::ACTION_NEED_RETRY => {
            }
            Err(error) => return Err(capture_err(error)),
        }
        if latest
            .as_ref()
            .is_some_and(|(_, token)| !token.is_current())
        {
            *latest = None;
        }
        let Some((frame, continuity)) = latest.as_ref() else {
            std::thread::sleep(Duration::from_millis(10));
            continue;
        };
        let (received, clock) = frame
            .received_at()
            .ok_or_else(|| internal("capture timestamp missing"))?;
        let (png, width, height) = encode_png_with_dimensions(frame.as_ref())?;
        enforce_size_limit(png.len(), MAX_IMAGE_BYTES)?;
        if stop.load(Ordering::Acquire)
            || session.closure_token().is_cancelled()
            || !request.is_current()
        {
            return Err(internal("Wayland snapshot ended during image encoding"));
        }
        if !continuity.is_current() {
            *latest = None;
            continue;
        }
        return Ok(Snapshot {
            output: ScreenCaptureOutput {
                display: super::super::display::reference(display),
                format: ImageFormat::Png,
                width,
                height,
                dpi_x: 96,
                dpi_y: 96,
                window: None,
                window_geometry: None,
                image: png,
                truncated: false,
                frame_observation: Some(ScreenFrameObservation {
                    observation_id: uuid::Uuid::new_v4().to_string(),
                    output_reference: None,
                    stream_generation: generation,
                    received_at_unix_ms: received
                        .duration_since(UNIX_EPOCH)
                        .map_err(|_| internal("capture clock invalid"))?
                        .as_millis() as u64,
                    receipt_age_ms: clock.elapsed().as_millis() as u64,
                    source_timestamp_ns: frame.source_timestamp_ns(),
                    freshness: ScreenFrameFreshness::LatestObserved,
                }),
            },
            continuity: continuity.clone(),
            received_at: clock,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelled_waiter_does_not_release_native_work_admission() {
        struct Admission(Arc<AtomicBool>);
        impl Drop for Admission {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }
        let released = Arc::new(AtomicBool::new(false));
        let (reply, receiver) = tokio::sync::oneshot::channel();
        let work = Work {
            deadline: Instant::now() + Duration::from_secs(60),
            reply,
            _permit: Admission(released.clone()),
        };
        assert!(work.is_current());
        drop(receiver);
        assert!(!work.is_current());
        assert!(!released.load(Ordering::Acquire));
        drop(work);
        assert!(released.load(Ordering::Acquire));
    }

    #[test]
    fn queued_deadline_expiry_prevents_capture_without_replaying() {
        let (reply, _receiver) = tokio::sync::oneshot::channel();
        let work = Work {
            deadline: Instant::now(),
            reply,
            _permit: (),
        };
        assert!(!work.is_current());
    }
}
