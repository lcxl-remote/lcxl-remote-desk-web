use std::sync::Arc;

use tokio::sync::mpsc;

use crate::{LivePortalSession, PortalError};

const INPUT_QUEUE_CAPACITY: usize = 1024;

enum InputEvent {
    PointerMotionAbsolute { x: f64, y: f64 },
    PointerButton { button: u32, state: u32 },
    PointerAxis { delta_x: f64, delta_y: f64 },
    KeyboardKeycode { keycode: i32, state: u32 },
}

#[derive(Clone)]
pub struct PortalInputSender {
    tx: mpsc::Sender<InputEvent>,
}

impl PortalInputSender {
    pub fn new(session: Arc<dyn LivePortalSession>) -> Self {
        let (tx, mut rx) = mpsc::channel(INPUT_QUEUE_CAPACITY);
        tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                let result = match event {
                    InputEvent::PointerMotionAbsolute { x, y } => {
                        session.notify_pointer_motion_absolute(x, y).await
                    }
                    InputEvent::PointerButton { button, state } => {
                        session.notify_pointer_button(button, state).await
                    }
                    InputEvent::PointerAxis { delta_x, delta_y } => {
                        session.notify_pointer_axis(delta_x, delta_y).await
                    }
                    InputEvent::KeyboardKeycode { keycode, state } => {
                        session.notify_keyboard_keycode(keycode, state).await
                    }
                };
                if let Err(error) = result {
                    log::warn!("Wayland Portal input notification failed: {error}");
                    break;
                }
            }
        });
        Self { tx }
    }

    pub fn notify_pointer_motion_absolute(&self, x: f64, y: f64) -> Result<(), PortalError> {
        self.send(InputEvent::PointerMotionAbsolute { x, y })
    }

    pub fn notify_pointer_button(&self, button: u32, state: u32) -> Result<(), PortalError> {
        self.send(InputEvent::PointerButton { button, state })
    }

    pub fn notify_pointer_axis(&self, delta_x: f64, delta_y: f64) -> Result<(), PortalError> {
        self.send(InputEvent::PointerAxis { delta_x, delta_y })
    }

    pub fn notify_keyboard_keycode(&self, keycode: i32, state: u32) -> Result<(), PortalError> {
        self.send(InputEvent::KeyboardKeycode { keycode, state })
    }

    fn send(&self, event: InputEvent) -> Result<(), PortalError> {
        self.tx.try_send(event).map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => {
                PortalError::Backend("Wayland Portal input queue is full".into())
            }
            mpsc::error::TrySendError::Closed(_) => PortalError::AuthorizationRequired,
        })
    }
}
