pub fn block_input(block: bool) -> Result<(), String> {
    use core_foundation::runloop::{CFRunLoop, kCFRunLoopCommonModes};
    use core_graphics::event::{
        CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType,
    };
    use std::sync::{Mutex, OnceLock, mpsc};
    use std::thread::{self, JoinHandle};

    struct InputBlocker {
        runloop: CFRunLoop,
        thread: JoinHandle<()>,
    }

    static INPUT_BLOCKER: OnceLock<Mutex<Option<InputBlocker>>> = OnceLock::new();

    fn blocker_slot() -> &'static Mutex<Option<InputBlocker>> {
        INPUT_BLOCKER.get_or_init(|| Mutex::new(None))
    }

    if block {
        let mut guard = blocker_slot()
            .lock()
            .map_err(|e| format!("Failed to acquire input blocker lock: {}", e))?;
        if guard.is_some() {
            return Ok(());
        }

        let (ready_tx, ready_rx) = mpsc::channel::<Result<CFRunLoop, String>>();
        let tap_thread = thread::spawn(move || {
            let events = vec![
                CGEventType::KeyDown,
                CGEventType::KeyUp,
                CGEventType::FlagsChanged,
                CGEventType::MouseMoved,
                CGEventType::LeftMouseDown,
                CGEventType::LeftMouseUp,
                CGEventType::LeftMouseDragged,
                CGEventType::RightMouseDown,
                CGEventType::RightMouseUp,
                CGEventType::RightMouseDragged,
                CGEventType::OtherMouseDown,
                CGEventType::OtherMouseUp,
                CGEventType::OtherMouseDragged,
                CGEventType::ScrollWheel,
            ];

            let tap = match CGEventTap::new(
                CGEventTapLocation::Session,
                CGEventTapPlacement::HeadInsertEventTap,
                CGEventTapOptions::Default,
                events,
                |_proxy, _event_type, event| {
                    event.set_type(CGEventType::Null);
                    Some(event.clone())
                },
            ) {
                Ok(tap) => tap,
                Err(_) => {
                    let _ = ready_tx.send(Err(
                        "Failed to create CGEventTap (Accessibility permission may be missing)"
                            .to_string(),
                    ));
                    return;
                }
            };

            let source = match tap.mach_port.create_runloop_source(0) {
                Ok(source) => source,
                Err(_) => {
                    let _ = ready_tx.send(Err("Failed to create run loop source".to_string()));
                    return;
                }
            };

            let current = CFRunLoop::get_current();
            current.add_source(&source, unsafe { kCFRunLoopCommonModes });
            tap.enable();

            let _ = ready_tx.send(Ok(current.clone()));
            log::info!("macOS input blocking started");
            CFRunLoop::run_current();
            log::info!("macOS input blocking stopped");
        });

        match ready_rx.recv() {
            Ok(Ok(runloop)) => {
                *guard = Some(InputBlocker {
                    runloop,
                    thread: tap_thread,
                });
            }
            Ok(Err(e)) => {
                // Best effort: do not fail private screen when local input interception fails.
                log::warn!(
                    "macOS block_input enable failed, continue without blocking: {}",
                    e
                );
            }
            Err(e) => {
                log::warn!(
                    "macOS block_input setup channel error, continue without blocking: {}",
                    e
                );
            }
        }
    } else {
        let mut guard = blocker_slot()
            .lock()
            .map_err(|e| format!("Failed to acquire input blocker lock: {}", e))?;
        if let Some(blocker) = guard.take() {
            blocker.runloop.stop();
            if blocker.thread.join().is_err() {
                log::warn!("Failed to join macOS input blocker thread");
            }
        }
    }

    Ok(())
}
