//! macOS input ownership monitor for Computer Use writer preemption.
//!
//! The passive session event tap ignores this product's marked remote input
//! events. Every unmarked or foreign event is conservatively treated as local
//! or third-party input and invalidates UI references and the active writer
//! lease. The marker is only a loop-avoidance label, not authorization.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use core_foundation::runloop::{CFRunLoop, kCFRunLoopCommonModes};
use core_graphics::event::{
    CGEvent, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType,
};
use desk_input_injection::macos_event::{REMOTE_INPUT_MARKER_FIELD, is_remote_input_marker};

use super::computer_use_broker::ComputerUseBroker;

pub struct MacosInputOwnershipMonitor {
    run_loop: CFRunLoop,
    join: Option<JoinHandle<()>>,
    broker: std::sync::Weak<ComputerUseBroker>,
}

impl MacosInputOwnershipMonitor {
    pub fn start(broker: &Arc<ComputerUseBroker>) -> Result<Self, String> {
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let broker = Arc::downgrade(broker);
        let thread_broker = broker.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        let thread_cancelled = Arc::clone(&cancelled);
        let join = thread::Builder::new()
            .name("computer-use-macos-input-owner".into())
            .spawn(move || {
                let callback_broker = thread_broker.clone();
                let tap = CGEventTap::new(
                    CGEventTapLocation::Session,
                    CGEventTapPlacement::HeadInsertEventTap,
                    CGEventTapOptions::ListenOnly,
                    observed_event_types(),
                    move |_proxy, event_type, event| {
                        note_if_external(&callback_broker, event_type, event);
                        None
                    },
                );
                let Ok(tap) = tap else {
                    let _ = ready_tx.send(Err(
                        "cannot install the macOS passive input event tap; Input Monitoring permission may be missing"
                            .into(),
                    ));
                    return;
                };
                let Ok(source) = tap.mach_port.create_runloop_source(0) else {
                    let _ = ready_tx.send(Err(
                        "cannot create the macOS input event-tap run-loop source".into(),
                    ));
                    return;
                };
                let run_loop = CFRunLoop::get_current();
                run_loop.add_source(&source, unsafe { kCFRunLoopCommonModes });
                tap.enable();
                if thread_cancelled.load(Ordering::SeqCst) {
                    return;
                }
                if ready_tx.send(Ok(run_loop.clone())).is_ok() {
                    CFRunLoop::run_current();
                }
            })
            .map_err(|error| format!("cannot start macOS input ownership thread: {error}"))?;

        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(run_loop)) => {
                if let Some(broker) = broker.upgrade() {
                    broker.set_input_ownership_ready(true);
                }
                Ok(Self {
                    run_loop,
                    join: Some(join),
                    broker,
                })
            }
            Ok(Err(error)) => {
                let _ = join.join();
                Err(error)
            }
            Err(error) => {
                cancelled.store(true, Ordering::SeqCst);
                Err(format!(
                    "macOS input ownership monitor did not initialize in time: {error}"
                ))
            }
        }
    }
}

impl Drop for MacosInputOwnershipMonitor {
    fn drop(&mut self) {
        if let Some(broker) = self.broker.upgrade() {
            broker.set_input_ownership_ready(false);
        }
        self.run_loop.stop();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn observed_event_types() -> Vec<CGEventType> {
    vec![
        CGEventType::LeftMouseDown,
        CGEventType::LeftMouseUp,
        CGEventType::RightMouseDown,
        CGEventType::RightMouseUp,
        CGEventType::OtherMouseDown,
        CGEventType::OtherMouseUp,
        CGEventType::MouseMoved,
        CGEventType::LeftMouseDragged,
        CGEventType::RightMouseDragged,
        CGEventType::OtherMouseDragged,
        CGEventType::ScrollWheel,
        CGEventType::KeyDown,
        CGEventType::KeyUp,
        CGEventType::FlagsChanged,
    ]
}

fn note_if_external(
    broker: &std::sync::Weak<ComputerUseBroker>,
    event_type: CGEventType,
    event: &CGEvent,
) {
    if matches!(
        event_type,
        CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput
    ) {
        if let Some(broker) = broker.upgrade() {
            broker.set_input_ownership_ready(false);
            broker.note_external_input();
        }
        return;
    }
    let marker = event.get_integer_value_field(REMOTE_INPUT_MARKER_FIELD);
    if !is_remote_input_marker(marker)
        && let Some(broker) = broker.upgrade()
    {
        broker.note_external_input();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monitor_covers_keyboard_mouse_drag_scroll_and_flag_events() {
        let events = observed_event_types();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, CGEventType::KeyDown))
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, CGEventType::MouseMoved))
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, CGEventType::ScrollWheel))
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, CGEventType::FlagsChanged))
        );
    }

    #[test]
    #[ignore = "requires a macOS Aqua session with Accessibility and Input Monitoring permission"]
    fn live_monitor_starts_and_stops() {
        let broker = Arc::new(ComputerUseBroker::new());
        let monitor = MacosInputOwnershipMonitor::start(&broker)
            .expect("the passive macOS input event tap must start");
        drop(monitor);
    }
}
