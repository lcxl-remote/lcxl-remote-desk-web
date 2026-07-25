//! macOS local input interception for the privacy screen.
//!
//! A session-level, head-inserted event tap sees physical keyboard, mouse and
//! trackpad input as well as the events `desk-input-injection` posts for the
//! remote controller — both travel the same Quartz stream. The tap therefore
//! classifies every event before deciding what to do with it:
//!
//! * events carrying the remote-input marker pass through untouched,
//! * the local `Ctrl+Alt+L` escape chord is consumed and reported to the
//!   privacy screen state machine,
//! * everything else physical is rewritten to `Null` so it never reaches an
//!   application,
//! * and the out-of-band notifications macOS sends when it disables a slow tap
//!   re-enable it instead of silently leaving the machine unprotected.

use super::LocalEscapeCallback;
use core_foundation::base::TCFType;
use core_foundation::mach_port::{CFMachPort, CFMachPortRef};
use core_foundation::runloop::{CFRunLoop, kCFRunLoopCommonModes};
use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventType, EventField,
};
use desk_input_injection::macos_event::{REMOTE_INPUT_MARKER_FIELD, is_remote_input_marker};
use std::cell::{OnceCell, RefCell};
use std::rc::Rc;
use std::sync::{Mutex, OnceLock, mpsc};
use std::thread::{self, JoinHandle};

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    /// Enables or disables an event tap identified by its mach port.
    ///
    /// `core-graphics` only exposes this through `CGEventTap::enable`, which
    /// borrows the whole tap. The tap callback cannot borrow the tap that owns
    /// it, so re-enabling from inside the callback goes through the mach port
    /// alone. Declared with an explicit framework link so it does not depend on
    /// another crate's optional `link` feature.
    unsafe fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
}

/// macOS virtual key code for `L`, the non-modifier half of the escape chord.
const ESCAPE_KEY_CODE: i64 = 0x25;

/// Modifiers the escape chord must carry.
const REQUIRED_MODIFIERS: u64 =
    CGEventFlags::CGEventFlagControl.bits() | CGEventFlags::CGEventFlagAlternate.bits();

/// Modifiers that disqualify a chord, so `Ctrl+Alt+Cmd+L` and `Ctrl+Alt+Shift+L`
/// stay ordinary suppressed input instead of dismissing the privacy screen.
const FORBIDDEN_MODIFIERS: u64 =
    CGEventFlags::CGEventFlagCommand.bits() | CGEventFlags::CGEventFlagShift.bits();

/// What the tap does with one observed event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TapAction {
    /// Remote input: deliver it unchanged to whatever application is focused.
    PassRemote,
    /// The local escape chord has been fully consumed; dismiss the privacy
    /// screen. The event itself is still suppressed.
    LocalEscape,
    /// macOS disabled the tap; turn it back on.
    ReEnable,
    /// Physical input while the privacy screen is up: rewrite it to `Null`.
    SuppressPhysical,
}

/// Which of the escape chord's two halves has been seen.
///
/// The chord is only acted on after both key down and key up have been
/// swallowed. Reacting on key down would tear the tap down while the key is
/// still held, leaving a stray `L` key up for the application underneath.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct EscapeChordTracker {
    armed: bool,
}

/// Event categories the decision distinguishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TapEventKind {
    /// An out-of-band `TapDisabledBy*` notification, not real input.
    Disabled,
    KeyDown,
    KeyUp,
    /// Any other intercepted keyboard or pointer event.
    Other,
}

/// The part of a Quartz event the decision depends on, as plain data so the
/// decision can be exercised without a window server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TapEventInput {
    kind: TapEventKind,
    user_data: i64,
    key_code: i64,
    flags: u64,
}

/// Whether the event's modifiers match the escape chord.
///
/// Only the four semantic modifier bits are inspected. Real keyboards also
/// report caps lock, Fn, numeric-pad and non-coalesced bits, and Quartz may
/// carry device-dependent bits on top; comparing the whole flag word would make
/// the escape hatch fail on hardware that sets any of them.
fn has_escape_modifiers(flags: u64) -> bool {
    flags & REQUIRED_MODIFIERS == REQUIRED_MODIFIERS && flags & FORBIDDEN_MODIFIERS == 0
}

/// Classify one observed event. The order is fixed:
///
/// 1. disable notifications, which carry no input payload,
/// 2. the remote marker, so a remote `Ctrl+Alt+L` reaches the remote session
///    instead of dismissing the local privacy screen,
/// 3. the local escape chord,
/// 4. suppression for everything else.
fn decide_tap_action(tracker: &mut EscapeChordTracker, event: TapEventInput) -> TapAction {
    if event.kind == TapEventKind::Disabled {
        // A half-finished chord must not survive the disabled window: the key
        // up that would complete it was never delivered to this tap.
        tracker.armed = false;
        return TapAction::ReEnable;
    }

    if is_remote_input_marker(event.user_data) {
        return TapAction::PassRemote;
    }

    match event.kind {
        TapEventKind::KeyDown
            if event.key_code == ESCAPE_KEY_CODE && has_escape_modifiers(event.flags) =>
        {
            // Key repeat re-arms an already armed chord, which is a no-op.
            tracker.armed = true;
            TapAction::SuppressPhysical
        }
        // The key up is matched on key code alone: releasing Ctrl or Alt before
        // `L` is a normal way to type the chord and must still work.
        TapEventKind::KeyUp if event.key_code == ESCAPE_KEY_CODE && tracker.armed => {
            tracker.armed = false;
            TapAction::LocalEscape
        }
        _ => TapAction::SuppressPhysical,
    }
}

/// Read the fields the decision needs off a live Quartz event.
fn read_tap_event(event_type: CGEventType, event: &CGEvent) -> TapEventInput {
    let kind = match event_type {
        CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput => {
            TapEventKind::Disabled
        }
        CGEventType::KeyDown => TapEventKind::KeyDown,
        CGEventType::KeyUp => TapEventKind::KeyUp,
        _ => TapEventKind::Other,
    };

    TapEventInput {
        kind,
        user_data: event.get_integer_value_field(REMOTE_INPUT_MARKER_FIELD),
        key_code: event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE),
        flags: event.get_flags().bits(),
    }
}

/// Rewrite an event to `Null` so it reaches no application.
///
/// `core-graphics` treats a `None` return as "keep the original event", so
/// dropping input requires returning a neutered event rather than nothing.
fn suppress(event: &CGEvent) -> Option<CGEvent> {
    event.set_type(CGEventType::Null);
    Some(event.clone())
}

/// Carry out a decision. Runs on the tap thread, so it stays non-blocking:
/// macOS disables taps whose callback is slow.
fn apply_tap_action(
    action: TapAction,
    event: &CGEvent,
    tap_port: &OnceCell<CFMachPort>,
    on_local_escape: Option<&LocalEscapeCallback>,
) -> Option<CGEvent> {
    match action {
        TapAction::PassRemote => None,
        TapAction::ReEnable => {
            log::warn!("macOS event tap was disabled by the system, re-enabling it");
            match tap_port.get() {
                Some(port) => unsafe { CGEventTapEnable(port.as_concrete_TypeRef(), true) },
                None => log::error!("macOS event tap port is unavailable, cannot re-enable"),
            }
            None
        }
        TapAction::LocalEscape => {
            match on_local_escape {
                Some(callback) => callback(),
                None => log::warn!("Local privacy screen escape chord observed with no handler"),
            }
            suppress(event)
        }
        TapAction::SuppressPhysical => suppress(event),
    }
}

/// A running interception: the tap thread and the run loop that drives it.
struct InputBlocker {
    runloop: CFRunLoop,
    thread: JoinHandle<()>,
}

static INPUT_BLOCKER: OnceLock<Mutex<Option<InputBlocker>>> = OnceLock::new();

fn blocker_slot() -> &'static Mutex<Option<InputBlocker>> {
    INPUT_BLOCKER.get_or_init(|| Mutex::new(None))
}

/// Event types the tap subscribes to. Disable notifications are delivered
/// regardless of the mask.
fn intercepted_event_types() -> Vec<CGEventType> {
    vec![
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
    ]
}

/// Start or stop intercepting local input.
///
/// Starting reports failure instead of degrading silently: the privacy screen
/// must not claim the machine is protected when physical input still reaches
/// it. `on_local_escape` is invoked from the tap thread after the local escape
/// chord has been fully consumed.
pub fn block_input(
    block: bool,
    on_local_escape: Option<LocalEscapeCallback>,
) -> Result<(), String> {
    if block {
        start_input_block(on_local_escape)
    } else {
        stop_input_block()
    }
}

fn start_input_block(on_local_escape: Option<LocalEscapeCallback>) -> Result<(), String> {
    let mut guard = blocker_slot()
        .lock()
        .map_err(|e| format!("Failed to acquire input blocker lock: {}", e))?;
    if guard.is_some() {
        return Ok(());
    }

    let (ready_tx, ready_rx) = mpsc::channel::<Result<CFRunLoop, String>>();
    let tap_thread = thread::spawn(move || {
        // The tap's mach port is needed inside the callback to re-enable the
        // tap, but it only exists once creation succeeds. A single-threaded
        // cell breaks that cycle without a self-referential struct.
        let tap_port: Rc<OnceCell<CFMachPort>> = Rc::new(OnceCell::new());
        let callback_port = Rc::clone(&tap_port);
        let tracker = RefCell::new(EscapeChordTracker::default());

        let tap = match CGEventTap::new(
            CGEventTapLocation::Session,
            CGEventTapPlacement::HeadInsertEventTap,
            CGEventTapOptions::Default,
            intercepted_event_types(),
            move |_proxy, event_type, event| {
                let input = read_tap_event(event_type, event);
                let action = decide_tap_action(&mut tracker.borrow_mut(), input);
                apply_tap_action(action, event, &callback_port, on_local_escape.as_ref())
            },
        ) {
            Ok(tap) => tap,
            Err(()) => {
                let _ = ready_tx.send(Err(
                    "Failed to create CGEventTap (Accessibility permission may be missing)"
                        .to_string(),
                ));
                return;
            }
        };

        if tap_port.set(tap.mach_port.clone()).is_err() {
            log::error!("macOS event tap port was already set");
        }

        let source = match tap.mach_port.create_runloop_source(0) {
            Ok(source) => source,
            Err(()) => {
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
            Ok(())
        }
        Ok(Err(e)) => {
            let _ = tap_thread.join();
            Err(e)
        }
        Err(e) => {
            let _ = tap_thread.join();
            Err(format!("macOS block_input setup channel error: {}", e))
        }
    }
}

fn stop_input_block() -> Result<(), String> {
    let mut guard = blocker_slot()
        .lock()
        .map_err(|e| format!("Failed to acquire input blocker lock: {}", e))?;
    if let Some(blocker) = guard.take() {
        blocker.runloop.stop();
        if blocker.thread.join().is_err() {
            log::warn!("Failed to join macOS input blocker thread");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHIFT: u64 = CGEventFlags::CGEventFlagShift.bits();
    const CONTROL: u64 = CGEventFlags::CGEventFlagControl.bits();
    const ALTERNATE: u64 = CGEventFlags::CGEventFlagAlternate.bits();
    const COMMAND: u64 = CGEventFlags::CGEventFlagCommand.bits();
    const CAPS_LOCK: u64 = CGEventFlags::CGEventFlagAlphaShift.bits();
    const NUMERIC_PAD: u64 = CGEventFlags::CGEventFlagNumericPad.bits();
    const SECONDARY_FN: u64 = CGEventFlags::CGEventFlagSecondaryFn.bits();
    const NON_COALESCED: u64 = CGEventFlags::CGEventFlagNonCoalesced.bits();
    /// A bit outside every documented `CGEventFlags` value, standing in for the
    /// device-dependent bits real keyboards attach.
    const DEVICE_SPECIFIC: u64 = 0x0000_0002;

    const REMOTE: i64 = desk_input_injection::macos_event::REMOTE_INPUT_MARKER;

    fn key(kind: TapEventKind, key_code: i64, flags: u64, user_data: i64) -> TapEventInput {
        TapEventInput {
            kind,
            user_data,
            key_code,
            flags,
        }
    }

    fn local_chord_down(flags: u64) -> TapEventInput {
        key(TapEventKind::KeyDown, ESCAPE_KEY_CODE, flags, 0)
    }

    fn local_chord_up() -> TapEventInput {
        key(TapEventKind::KeyUp, ESCAPE_KEY_CODE, 0, 0)
    }

    fn disabled() -> TapEventInput {
        key(TapEventKind::Disabled, 0, 0, 0)
    }

    #[test]
    fn tap_disabled_notifications_re_enable_the_tap() {
        let mut tracker = EscapeChordTracker::default();
        assert_eq!(
            decide_tap_action(&mut tracker, disabled()),
            TapAction::ReEnable
        );
    }

    #[test]
    fn marked_events_pass_through() {
        let mut tracker = EscapeChordTracker::default();
        assert_eq!(
            decide_tap_action(
                &mut tracker,
                key(TapEventKind::Other, 0, NON_COALESCED, REMOTE)
            ),
            TapAction::PassRemote
        );
        assert_eq!(
            decide_tap_action(&mut tracker, key(TapEventKind::KeyDown, 0x00, 0, REMOTE)),
            TapAction::PassRemote
        );
    }

    /// A remote `Ctrl+Alt+L` is an ordinary remote chord: it must reach the
    /// remote session and must never dismiss the local privacy screen.
    #[test]
    fn marked_escape_chord_is_still_remote_input() {
        let mut tracker = EscapeChordTracker::default();
        let down = key(
            TapEventKind::KeyDown,
            ESCAPE_KEY_CODE,
            CONTROL | ALTERNATE,
            REMOTE,
        );
        let up = key(TapEventKind::KeyUp, ESCAPE_KEY_CODE, 0, REMOTE);

        assert_eq!(decide_tap_action(&mut tracker, down), TapAction::PassRemote);
        assert_eq!(decide_tap_action(&mut tracker, up), TapAction::PassRemote);
        assert!(
            !tracker.armed,
            "remote input must not arm the local escape chord"
        );
    }

    #[test]
    fn local_escape_chord_is_consumed_across_down_and_up() {
        let mut tracker = EscapeChordTracker::default();

        assert_eq!(
            decide_tap_action(&mut tracker, local_chord_down(CONTROL | ALTERNATE)),
            TapAction::SuppressPhysical,
            "key down only arms the chord"
        );
        assert!(tracker.armed);
        assert_eq!(
            decide_tap_action(&mut tracker, local_chord_up()),
            TapAction::LocalEscape
        );
        assert!(!tracker.armed);
    }

    /// Key repeat must not produce a second dismissal, and the trailing key up
    /// must still be swallowed exactly once.
    #[test]
    fn key_repeat_does_not_dismiss_twice() {
        let mut tracker = EscapeChordTracker::default();
        for _ in 0..5 {
            assert_eq!(
                decide_tap_action(&mut tracker, local_chord_down(CONTROL | ALTERNATE)),
                TapAction::SuppressPhysical
            );
        }
        assert_eq!(
            decide_tap_action(&mut tracker, local_chord_up()),
            TapAction::LocalEscape
        );
        assert_eq!(
            decide_tap_action(&mut tracker, local_chord_up()),
            TapAction::SuppressPhysical,
            "a second key up has no armed chord left to complete"
        );
    }

    /// Extra hardware flags must not break the escape hatch.
    #[test]
    fn extra_non_semantic_flags_still_match() {
        let mut tracker = EscapeChordTracker::default();
        let flags = CONTROL
            | ALTERNATE
            | CAPS_LOCK
            | NUMERIC_PAD
            | SECONDARY_FN
            | NON_COALESCED
            | DEVICE_SPECIFIC;

        assert_eq!(
            decide_tap_action(&mut tracker, local_chord_down(flags)),
            TapAction::SuppressPhysical
        );
        assert_eq!(
            decide_tap_action(&mut tracker, local_chord_up()),
            TapAction::LocalEscape
        );
    }

    #[test]
    fn near_miss_chords_do_not_dismiss() {
        for flags in [
            CONTROL,                       // Alt missing
            ALTERNATE,                     // Ctrl missing
            CONTROL | ALTERNATE | COMMAND, // Command disqualifies
            CONTROL | ALTERNATE | SHIFT,   // Shift disqualifies
            CONTROL | SHIFT,               // neither required pair
            CONTROL | ALTERNATE | COMMAND | SHIFT,
        ] {
            let mut tracker = EscapeChordTracker::default();
            assert_eq!(
                decide_tap_action(&mut tracker, local_chord_down(flags)),
                TapAction::SuppressPhysical
            );
            assert!(!tracker.armed, "flags {flags:#x} must not arm the chord");
            assert_eq!(
                decide_tap_action(&mut tracker, local_chord_up()),
                TapAction::SuppressPhysical
            );
        }
    }

    /// The right modifiers on the wrong key must not dismiss either.
    #[test]
    fn another_key_with_escape_modifiers_is_suppressed() {
        let mut tracker = EscapeChordTracker::default();
        let other_key = key(TapEventKind::KeyDown, 0x00, CONTROL | ALTERNATE, 0);

        assert_eq!(
            decide_tap_action(&mut tracker, other_key),
            TapAction::SuppressPhysical
        );
        assert!(!tracker.armed);
    }

    #[test]
    fn unmarked_physical_input_is_suppressed() {
        let mut tracker = EscapeChordTracker::default();
        for event in [
            key(TapEventKind::Other, 0, 0, 0),
            key(TapEventKind::KeyDown, 0x0B, 0, 0),
            key(TapEventKind::KeyUp, 0x0B, 0, 0),
            key(TapEventKind::Other, 0, 0, 1),
        ] {
            assert_eq!(
                decide_tap_action(&mut tracker, event),
                TapAction::SuppressPhysical
            );
        }
    }

    /// A tap disable between key down and key up drops the half-consumed chord,
    /// so the key up that arrives after re-enabling cannot dismiss on its own.
    #[test]
    fn disable_notification_clears_a_half_consumed_chord() {
        let mut tracker = EscapeChordTracker::default();

        decide_tap_action(&mut tracker, local_chord_down(CONTROL | ALTERNATE));
        assert!(tracker.armed);

        assert_eq!(
            decide_tap_action(&mut tracker, disabled()),
            TapAction::ReEnable
        );
        assert!(!tracker.armed);

        assert_eq!(
            decide_tap_action(&mut tracker, local_chord_up()),
            TapAction::SuppressPhysical
        );
    }

    #[test]
    fn escape_modifier_mask_ignores_non_semantic_bits() {
        assert!(has_escape_modifiers(CONTROL | ALTERNATE));
        assert!(has_escape_modifiers(
            CONTROL | ALTERNATE | CAPS_LOCK | SECONDARY_FN
        ));
        assert!(!has_escape_modifiers(CONTROL | ALTERNATE | COMMAND));
        assert!(!has_escape_modifiers(CONTROL | ALTERNATE | SHIFT));
        assert!(!has_escape_modifiers(CONTROL));
        assert!(!has_escape_modifiers(0));
    }
}
