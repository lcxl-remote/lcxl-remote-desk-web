use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use crate::{LivePortalSession, PortalError};

mod completion;
mod pressed;
use completion::GuardedInput;
pub use completion::{PortalInputFailure, PortalInputReceipt};

enum QueuedInput {
    Interactive(PortalInputEvent),
    Guarded(GuardedInput),
}

const INPUT_QUEUE_CAPACITY: usize = 1024;
const INPUT_ERROR_WARN_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub enum PortalInputEvent {
    PointerMotionAbsolute { x: f64, y: f64 },
    PointerButton { button: i32, state: u32 },
    PointerAxis { delta_x: f64, delta_y: f64 },
    KeyboardKeycode { keycode: i32, state: u32 },
    KeyboardKeysym { keysym: i32, state: u32 },
}

#[derive(Clone)]
pub struct PortalInputSender {
    tx: mpsc::Sender<QueuedInput>,
    terminal_error: Arc<Mutex<Option<String>>>,
}

impl PortalInputSender {
    pub fn new(session: Arc<dyn LivePortalSession>) -> Self {
        let (tx, mut rx) = mpsc::channel(INPUT_QUEUE_CAPACITY);
        let terminal_error = Arc::new(Mutex::new(None));
        let task_error = terminal_error.clone();
        let session_closed = session.closure_token();
        tokio::spawn(async move {
            let mut last_error_warn_at = None;
            let mut suppressed_errors = 0_u64;
            let mut interactive_pressed = pressed::Pressed::default();
            loop {
                let event = tokio::select! {
                    biased;
                    _ = session_closed.cancelled() => {
                        *task_error
                            .lock()
                            .expect("Wayland Portal input error lock poisoned") =
                            Some("Wayland Portal session closed".into());
                        break;
                    }
                    event = rx.recv() => {
                        let Some(event) = event else {
                            break;
                        };
                        event
                    }
                };
                let result = match event {
                    QueuedInput::Interactive(event) => {
                        interactive_pressed.before(&event);
                        let result = dispatch(session.as_ref(), event.clone()).await;
                        if result.is_ok() {
                            interactive_pressed.after(&event);
                        }
                        result
                    }
                    QueuedInput::Guarded(request) => {
                        if !interactive_pressed.is_empty() {
                            request.reject_interactive_hold();
                            continue;
                        }
                        if request.execute(session.as_ref()).await {
                            *task_error
                                .lock()
                                .expect("Wayland Portal input error lock poisoned") = Some(
                                "Wayland Portal input session retired after an unknown outcome"
                                    .into(),
                            );
                            break;
                        }
                        continue;
                    }
                };
                if let Err(error) = result {
                    let now = Instant::now();
                    let should_warn = last_error_warn_at
                        .is_none_or(|last| now.duration_since(last) >= INPUT_ERROR_WARN_INTERVAL);
                    if should_warn {
                        log::warn!(
                            "Wayland Portal input notification failed: {error}; suppressed {} similar errors",
                            suppressed_errors
                        );
                        last_error_warn_at = Some(now);
                        suppressed_errors = 0;
                    } else {
                        suppressed_errors = suppressed_errors.saturating_add(1);
                    }
                } else {
                    last_error_warn_at = None;
                    suppressed_errors = 0;
                }
            }
        });
        Self { tx, terminal_error }
    }

    pub fn notify_pointer_motion_absolute(&self, x: f64, y: f64) -> Result<(), PortalError> {
        self.send(PortalInputEvent::PointerMotionAbsolute { x, y })
    }

    pub fn notify_pointer_button(&self, button: i32, state: u32) -> Result<(), PortalError> {
        self.send(PortalInputEvent::PointerButton { button, state })
    }

    pub fn notify_pointer_axis(&self, delta_x: f64, delta_y: f64) -> Result<(), PortalError> {
        self.send(PortalInputEvent::PointerAxis { delta_x, delta_y })
    }

    pub fn notify_keyboard_keycode(&self, keycode: i32, state: u32) -> Result<(), PortalError> {
        self.send(PortalInputEvent::KeyboardKeycode { keycode, state })
    }

    fn send(&self, event: PortalInputEvent) -> Result<(), PortalError> {
        self.tx
            .try_send(QueuedInput::Interactive(event))
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => {
                    PortalError::Backend("Wayland Portal input queue is full".into())
                }
                mpsc::error::TrySendError::Closed(_) => {
                    let reason = self
                        .terminal_error
                        .lock()
                        .expect("Wayland Portal input error lock poisoned")
                        .clone()
                        .unwrap_or_else(|| "Wayland Portal input worker stopped".into());
                    PortalError::Backend(reason)
                }
            })
    }
}

async fn dispatch(
    session: &dyn LivePortalSession,
    event: PortalInputEvent,
) -> Result<(), PortalError> {
    match event {
        PortalInputEvent::PointerMotionAbsolute { x, y } => {
            session.notify_pointer_motion_absolute(x, y).await
        }
        PortalInputEvent::PointerButton { button, state } => {
            session.notify_pointer_button(button, state).await
        }
        PortalInputEvent::PointerAxis { delta_x, delta_y } => {
            session.notify_pointer_axis(delta_x, delta_y).await
        }
        PortalInputEvent::KeyboardKeycode { keycode, state } => {
            session.notify_keyboard_keycode(keycode, state).await
        }
        PortalInputEvent::KeyboardKeysym { keysym, state } => {
            session.notify_keyboard_keysym(keysym, state).await
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use async_trait::async_trait;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::{AuthorizationTarget, PortalStream};

    struct FakeSession {
        stream: PortalStream,
        buttons: StdMutex<Vec<(i32, u32)>>,
        motions: AtomicUsize,
        fail_buttons: bool,
        closed: CancellationToken,
    }

    impl FakeSession {
        fn new(fail_buttons: bool) -> Self {
            Self {
                stream: PortalStream {
                    node_id: 1,
                    id: None,
                    position: None,
                    size: None,
                    mapping_id: None,
                },
                buttons: StdMutex::new(Vec::new()),
                motions: AtomicUsize::new(0),
                fail_buttons,
                closed: CancellationToken::new(),
            }
        }
    }

    #[async_trait]
    impl LivePortalSession for FakeSession {
        fn target(&self) -> AuthorizationTarget {
            AuthorizationTarget::ScreenAndInput
        }

        fn stream(&self) -> &PortalStream {
            &self.stream
        }

        #[cfg(target_os = "linux")]
        fn duplicate_pipewire_fd(&self) -> Result<std::os::fd::OwnedFd, PortalError> {
            Err(PortalError::Backend(
                "fake session has no PipeWire fd".into(),
            ))
        }

        async fn notify_pointer_motion_absolute(&self, _: f64, _: f64) -> Result<(), PortalError> {
            self.motions.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        async fn notify_pointer_button(&self, button: i32, state: u32) -> Result<(), PortalError> {
            self.buttons
                .lock()
                .expect("fake button lock poisoned")
                .push((button, state));
            if self.fail_buttons {
                Err(PortalError::Backend("injected button failure".into()))
            } else {
                Ok(())
            }
        }

        async fn notify_pointer_axis(&self, _: f64, _: f64) -> Result<(), PortalError> {
            Ok(())
        }

        async fn notify_keyboard_keycode(&self, _: i32, _: u32) -> Result<(), PortalError> {
            Ok(())
        }

        fn closure_token(&self) -> CancellationToken {
            self.closed.clone()
        }

        async fn close(&self) -> Result<(), PortalError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn guarded_input_returns_native_failure_as_possibly_started() {
        let session = Arc::new(FakeSession::new(true));
        let sender = PortalInputSender::new(session.clone());
        let failure = sender
            .submit_guarded_batch(
                vec![
                    PortalInputEvent::PointerButton {
                        button: 0x110,
                        state: 1,
                    },
                    PortalInputEvent::PointerButton {
                        button: 0x110,
                        state: 0,
                    },
                ],
                Duration::from_secs(1),
                CancellationToken::new(),
                || Ok(()),
            )
            .await
            .unwrap_err();
        assert!(failure.possibly_started);
        assert_eq!(
            *session.buttons.lock().unwrap(),
            vec![(0x110, 1), (0x110, 0)]
        );
        assert!(session.closed.is_cancelled());
    }

    #[tokio::test]
    async fn batch_revocation_releases_its_press_and_skips_following_actions() {
        let session = Arc::new(FakeSession::new(false));
        let sender = PortalInputSender::new(session.clone());
        let checks = AtomicUsize::new(0);
        let result = sender
            .submit_guarded_batch(
                vec![
                    PortalInputEvent::PointerButton {
                        button: 0x110,
                        state: 1,
                    },
                    PortalInputEvent::PointerMotionAbsolute { x: 2.0, y: 3.0 },
                    PortalInputEvent::PointerButton {
                        button: 0x110,
                        state: 0,
                    },
                ],
                Duration::from_secs(1),
                CancellationToken::new(),
                move || {
                    if checks.fetch_add(1, Ordering::SeqCst) == 0 {
                        Ok(())
                    } else {
                        Err(PortalError::Cancelled)
                    }
                },
            )
            .await
            .unwrap_err();
        assert!(result.possibly_started);
        assert_eq!(
            *session.buttons.lock().unwrap(),
            vec![(0x110, 1), (0x110, 0)]
        );
        assert_eq!(session.motions.load(Ordering::Relaxed), 0);
        assert!(!session.closed.is_cancelled());
    }

    #[tokio::test]
    async fn batch_does_not_release_interactive_held_button() {
        let session = Arc::new(FakeSession::new(false));
        let sender = PortalInputSender::new(session.clone());
        sender.notify_pointer_button(0x110, 1).unwrap();
        let result = sender
            .submit_guarded(
                PortalInputEvent::PointerMotionAbsolute { x: 2.0, y: 3.0 },
                Duration::from_secs(1),
                CancellationToken::new(),
                || Ok(()),
            )
            .await
            .unwrap_err();
        assert!(!result.possibly_started);
        assert_eq!(*session.buttons.lock().unwrap(), vec![(0x110, 1)]);
        sender.notify_pointer_button(0x110, 0).unwrap();
        sender
            .submit_guarded(
                PortalInputEvent::PointerMotionAbsolute { x: 2.0, y: 3.0 },
                Duration::from_secs(1),
                CancellationToken::new(),
                || Ok(()),
            )
            .await
            .unwrap();
        assert_eq!(session.motions.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn cancelled_guarded_input_never_reaches_portal() {
        let session = Arc::new(FakeSession::new(false));
        let sender = PortalInputSender::new(session.clone());
        let cancel = CancellationToken::new();
        cancel.cancel();
        let failure = sender
            .submit_guarded(
                PortalInputEvent::PointerMotionAbsolute { x: 1.0, y: 2.0 },
                Duration::from_secs(1),
                cancel,
                || Ok(()),
            )
            .await
            .unwrap_err();
        assert!(!failure.possibly_started);
        // The following receipt also acts as a queue barrier.
        sender
            .submit_guarded(
                PortalInputEvent::PointerAxis {
                    delta_x: 0.0,
                    delta_y: 1.0,
                },
                Duration::from_secs(1),
                CancellationToken::new(),
                || Ok(()),
            )
            .await
            .unwrap();
        assert_eq!(session.motions.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn revoked_authority_prevents_native_dispatch() {
        let session = Arc::new(FakeSession::new(false));
        let sender = PortalInputSender::new(session.clone());
        let failure = sender
            .submit_guarded(
                PortalInputEvent::PointerMotionAbsolute { x: 1.0, y: 2.0 },
                Duration::from_secs(1),
                CancellationToken::new(),
                || Err(PortalError::Cancelled),
            )
            .await
            .unwrap_err();
        assert!(!failure.possibly_started);
        assert_eq!(session.motions.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn pointer_button_uses_signed_portal_button_code() {
        let session = Arc::new(FakeSession::new(false));
        let sender = PortalInputSender::new(session.clone());

        sender
            .notify_pointer_button(0x110, 1)
            .expect("queue button");

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if session
                    .buttons
                    .lock()
                    .expect("fake button lock poisoned")
                    .as_slice()
                    == [(0x110, 1)]
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("button delivered");
    }

    #[tokio::test]
    async fn one_failed_event_does_not_stop_input_worker() {
        let session = Arc::new(FakeSession::new(true));
        let sender = PortalInputSender::new(session.clone());
        sender
            .notify_pointer_button(0x110, 1)
            .expect("queue button");
        sender
            .notify_pointer_motion_absolute(1.0, 1.0)
            .expect("queue motion after failed button");

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if session.motions.load(Ordering::Relaxed) == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("motion delivered after one failed event");
    }

    #[tokio::test]
    async fn closed_session_stops_input_worker_with_backend_failure() {
        let session = Arc::new(FakeSession::new(false));
        let sender = PortalInputSender::new(session.clone());
        session.closed.cancel();

        tokio::time::timeout(Duration::from_secs(1), sender.tx.closed())
            .await
            .expect("input queue closes after session closes");

        let error = sender
            .notify_pointer_motion_absolute(1.0, 1.0)
            .expect_err("input worker stops after session closes");

        assert!(matches!(error, PortalError::Backend(reason) if reason.contains("session closed")));
    }
}
