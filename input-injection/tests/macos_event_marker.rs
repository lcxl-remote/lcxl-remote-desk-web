#![cfg(target_os = "macos")]
//! Manual, machine-bound check that the remote-input marker survives a real
//! round trip through Quartz.
//!
//! The privacy screen relies on a session event tap reading back the value this
//! crate writes into `EVENT_SOURCE_USER_DATA` before posting to the HID tap
//! location. Nothing in the public CoreGraphics contract promises that a
//! user-data field written by the poster is still present when a tap installed
//! at a different location observes the event, so the assumption is verified
//! here against the real system rather than assumed.
//!
//! The test is `#[ignore]` because it needs a logged-in GUI session and the
//! running binary must hold Accessibility / Input Monitoring approval. The test
//! binary's TCC identity differs from the shipped app, so a failure here is not
//! automatically a product defect — the three stages below exist to keep a
//! permission or environment failure from being misread as "Quartz stripped the
//! marker".
//!
//! ```bash
//! cargo test -p desk-input-injection --test macos_event_marker -- --ignored --nocapture
//! ```

use core_foundation::runloop::{CFRunLoop, kCFRunLoopCommonModes};
use core_graphics::event::{
    CGEvent, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::CGPoint;
use desk_input_injection::macos_event::{
    REMOTE_INPUT_MARKER, REMOTE_INPUT_MARKER_FIELD, is_remote_input_marker, post_remote_input_event,
};
use std::sync::mpsc;
use std::time::Duration;

/// How long the tap run loop is allowed to wait for the two probe events.
const OBSERVE_TIMEOUT: Duration = Duration::from_secs(5);

/// What the tap observed for one posted probe event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Observed {
    /// An event with a zero user-data field: the unmarked baseline probe.
    Unmarked,
    /// An event carrying exactly the remote-input marker.
    Marked,
    /// An event with some other user-data value; reported verbatim so a
    /// partially preserved or rewritten field is distinguishable from removal.
    Foreign(i64),
}

#[test]
#[ignore = "requires a GUI session plus Accessibility/Input Monitoring approval"]
fn remote_input_marker_survives_hid_post() {
    // Stage 1: can this binary create the same tap the privacy screen uses?
    let (ready_tx, ready_rx) = mpsc::channel::<Result<CFRunLoop, String>>();
    let (seen_tx, seen_rx) = mpsc::channel::<Observed>();

    let tap_thread = std::thread::spawn(move || {
        let tap = match CGEventTap::new(
            CGEventTapLocation::Session,
            CGEventTapPlacement::HeadInsertEventTap,
            CGEventTapOptions::Default,
            vec![CGEventType::MouseMoved],
            move |_proxy, _event_type, event| {
                let user_data = event.get_integer_value_field(REMOTE_INPUT_MARKER_FIELD);
                let observed = if is_remote_input_marker(user_data) {
                    Observed::Marked
                } else if user_data == 0 {
                    Observed::Unmarked
                } else {
                    Observed::Foreign(user_data)
                };
                let _ = seen_tx.send(observed);
                // Pass every event through untouched: this probe must not
                // disturb the machine it runs on.
                None
            },
        ) {
            Ok(tap) => tap,
            Err(()) => {
                let _ = ready_tx.send(Err(
                    "CGEventTap::new failed — the test binary most likely lacks \
                     Accessibility/Input Monitoring approval, or there is no GUI session"
                        .to_string(),
                ));
                return;
            }
        };

        let source = match tap.mach_port.create_runloop_source(0) {
            Ok(source) => source,
            Err(()) => {
                let _ = ready_tx.send(Err("failed to create run loop source".to_string()));
                return;
            }
        };

        let run_loop = CFRunLoop::get_current();
        run_loop.add_source(&source, unsafe { kCFRunLoopCommonModes });
        tap.enable();

        let _ = ready_tx.send(Ok(run_loop.clone()));
        CFRunLoop::run_current();
    });

    let run_loop = match ready_rx.recv() {
        Ok(Ok(run_loop)) => run_loop,
        Ok(Err(reason)) => {
            let _ = tap_thread.join();
            panic!(
                "stage 1 (tap creation) failed: {reason}. \
                 No conclusion about marker preservation can be drawn from this run."
            );
        }
        Err(error) => {
            let _ = tap_thread.join();
            panic!("stage 1 (tap creation) failed: setup channel closed: {error}");
        }
    };
    println!("stage 1 ok: session tap created and enabled");

    // Stage 2: does this tap observe events posted by this very process? A
    // silent tap means the probe never reached Quartz, which says nothing about
    // the marker.
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .expect("stage 2 (baseline echo): CGEventSource creation failed");
    let cursor = CGEvent::new(source.clone())
        .map(|event| event.location())
        .unwrap_or(CGPoint { x: 100.0, y: 100.0 });

    let baseline = CGEvent::new_mouse_event(
        source.clone(),
        CGEventType::MouseMoved,
        cursor,
        core_graphics::event::CGMouseButton::Left,
    )
    .expect("stage 2 (baseline echo): mouse event creation failed");
    baseline.post(CGEventTapLocation::HID);

    let baseline_seen = seen_rx.recv_timeout(OBSERVE_TIMEOUT);
    let baseline_seen = match baseline_seen {
        Ok(observed) => observed,
        Err(error) => {
            run_loop.stop();
            let _ = tap_thread.join();
            panic!(
                "stage 2 (baseline echo) failed: the tap never observed an unmarked event \
                 this process posted ({error}). This is a permission, run loop or event-type \
                 problem, NOT evidence that the marker is stripped."
            );
        }
    };
    assert_eq!(
        baseline_seen,
        Observed::Unmarked,
        "stage 2 (baseline echo): an unmarked probe must arrive with a zero user-data field"
    );
    println!("stage 2 ok: unmarked probe observed with user data 0");

    // Stage 3: the actual question — does the marker written before posting
    // still reach a tap on the same path?
    let marked = CGEvent::new_mouse_event(
        source,
        CGEventType::MouseMoved,
        cursor,
        core_graphics::event::CGMouseButton::Left,
    )
    .expect("stage 3 (marker preservation): mouse event creation failed");
    post_remote_input_event(&marked);

    let marked_seen = seen_rx.recv_timeout(OBSERVE_TIMEOUT);
    run_loop.stop();
    let _ = tap_thread.join();

    match marked_seen {
        Ok(Observed::Marked) => {
            println!("stage 3 ok: marker {REMOTE_INPUT_MARKER:#x} preserved through HID post");
        }
        Ok(Observed::Unmarked) => panic!(
            "stage 3 (marker preservation) FAILED: the marker was cleared between post and tap. \
             Do not migrate injection onto this marker; pick another cross-process identification."
        ),
        Ok(Observed::Foreign(value)) => panic!(
            "stage 3 (marker preservation) FAILED: user data arrived as {value:#x}, \
             expected {REMOTE_INPUT_MARKER:#x}"
        ),
        Err(error) => panic!(
            "stage 3 (marker preservation) inconclusive: the marked probe was never observed \
             ({error}), even though stage 2 succeeded"
        ),
    }
}
