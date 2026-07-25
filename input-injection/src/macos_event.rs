//! Source tagging for the macOS events this crate injects.
//!
//! Remote input is synthesized with `CGEvent::post(CGEventTapLocation::HID)`,
//! which puts it on the same Quartz stream that carries physical keyboard,
//! mouse and trackpad input. The privacy screen installs a session-level event
//! tap that has to suppress physical input while letting remote input through,
//! and a tap callback only ever sees the event — not the process that created
//! it. Every event posted from here therefore carries a fixed value in
//! `EVENT_SOURCE_USER_DATA` (field 42) so the tap can tell the two apart.
//!
//! The marker is a classification label, not a security boundary. Any process
//! that already holds Accessibility permission can synthesize input and set the
//! same field; the tap only uses it to decide which events belong to this
//! product's remote session.

use core_graphics::event::{CGEvent, CGEventTapLocation, EventField};

/// Value written into `EVENT_SOURCE_USER_DATA` on every injected event.
///
/// Chosen to be non-zero and specific enough that an unrelated producer is
/// unlikely to collide with it by accident: ASCII `LCXLRD` followed by a
/// version byte pair. The default value of the field is `0`, so untouched
/// physical events never match.
pub const REMOTE_INPUT_MARKER: i64 = 0x4C43_584C_5244_0001;

/// The marker has to stay non-zero and positive so it survives both the signed
/// and unsigned readings of the Quartz field.
const _: () = assert!(REMOTE_INPUT_MARKER > 0);

/// The Quartz event field the marker lives in.
///
/// Exposed so the event tap can read the same field without re-deriving the
/// numeric constant.
pub const REMOTE_INPUT_MARKER_FIELD: u32 = EventField::EVENT_SOURCE_USER_DATA;

/// Whether a raw `EVENT_SOURCE_USER_DATA` value identifies an event injected by
/// this crate. The comparison is exact: near-miss values are treated as foreign.
pub fn is_remote_input_marker(user_data: i64) -> bool {
    user_data == REMOTE_INPUT_MARKER
}

/// Stamp the remote-input marker onto an event that is about to be posted.
pub fn mark_remote_input_event(event: &CGEvent) {
    event.set_integer_value_field(REMOTE_INPUT_MARKER_FIELD, REMOTE_INPUT_MARKER);
}

/// Post an injected event onto the HID tap location after marking it.
///
/// Every mouse, drag, scroll and keyboard injection in this crate goes through
/// this helper. Posting directly would produce an unmarked event that the
/// privacy screen tap cannot distinguish from physical input and would drop.
pub fn post_remote_input_event(event: &CGEvent) {
    mark_remote_input_event(event);
    event.post(CGEventTapLocation::HID);
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};

    /// The field defaults to zero on physical events, so zero must never be
    /// mistaken for an injected event.
    #[test]
    fn default_field_value_is_not_a_marker() {
        assert!(!is_remote_input_marker(0));
    }

    /// Matching is exact — an off-by-one value is a different producer.
    #[test]
    fn near_miss_values_are_not_markers() {
        assert!(!is_remote_input_marker(REMOTE_INPUT_MARKER - 1));
        assert!(!is_remote_input_marker(REMOTE_INPUT_MARKER + 1));
        assert!(!is_remote_input_marker(-REMOTE_INPUT_MARKER));
        assert!(!is_remote_input_marker(i64::MAX));
    }

    #[test]
    fn the_marker_value_itself_matches() {
        assert!(is_remote_input_marker(REMOTE_INPUT_MARKER));
    }

    /// Marking writes the agreed field, and reading it back identifies the
    /// event. Creating a `CGEventSource` needs a WindowServer connection; when
    /// that is unavailable (headless build agent) the round trip cannot be
    /// exercised, so the test reports why it did nothing instead of asserting
    /// on an event it could not create.
    #[test]
    fn marking_an_event_makes_it_recognizable() {
        let Ok(source) = CGEventSource::new(CGEventSourceStateID::HIDSystemState) else {
            eprintln!("skipped: no WindowServer connection for CGEventSource");
            return;
        };
        let Ok(event) = CGEvent::new_keyboard_event(source, 0x25, true) else {
            eprintln!("skipped: CGEvent creation unavailable");
            return;
        };

        assert!(!is_remote_input_marker(
            event.get_integer_value_field(REMOTE_INPUT_MARKER_FIELD)
        ));

        mark_remote_input_event(&event);

        assert!(is_remote_input_marker(
            event.get_integer_value_field(REMOTE_INPUT_MARKER_FIELD)
        ));
    }
}
