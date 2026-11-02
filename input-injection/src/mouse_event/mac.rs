use crate::{
    error::InputError,
    macos_event::post_remote_input_event,
    model::{
        data_channel::{MouseEventData, MouseEventHandler},
        geometry::SharedMonitorGeometry,
    },
};
use core_graphics::event::{CGEvent, CGEventType, CGMouseButton, EventField, ScrollEventUnit};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::CGPoint;
use desk_utils::error::DeskErrorCode;
use objc2_app_kit::NSEvent;
use std::time::{Duration, Instant};

pub struct MacMouseEventHandler {
    /// Shared, hot-updatable display rect for the captured monitor.
    /// `CGEvent::post(CGEventTapLocation::HID)` reads `CGPoint`s in the
    /// global multi-display coordinate space, so non-primary displays
    /// require a non-zero offset to land the cursor on the right panel.
    /// The worker mutates this on display reconfiguration.
    geometry: SharedMonitorGeometry,
    /// Core Graphics accepts integer pixel deltas. Preserve fractional
    /// trackpad movement until it adds up to a whole pixel instead of dropping
    /// every sub-pixel event during the `f64` to `i32` conversion.
    wheel_acc_x: f64,
    wheel_acc_y: f64,
    clicks: ClickTracker,
}

impl MacMouseEventHandler {
    pub fn new(geometry: SharedMonitorGeometry) -> Result<Self, InputError> {
        Ok(Self {
            geometry,
            wheel_acc_x: 0.0,
            wheel_acc_y: 0.0,
            clicks: ClickTracker::default(),
        })
    }

    fn get_point(&self, x: f64, y: f64) -> CGPoint {
        // Snapshot the geometry into locals so the unsafe Core Graphics
        // calls below never run with the lock held.
        let (left, top, width, height) = {
            let g = self.geometry.read().expect("monitor geometry poisoned");
            (g.left, g.top, g.width, g.height)
        };
        let (px, py) = compute_absolute_f64(left, top, width, height, x, y);
        CGPoint { x: px, y: py }
    }

    fn create_source() -> Result<CGEventSource, InputError> {
        match CGEventSource::new(CGEventSourceStateID::HIDSystemState) {
            Ok(source) => Ok(source),
            Err(_) => InputError::custom_error(
                DeskErrorCode::SYSTEM_ERROR,
                "Failed to create event source",
            ),
        }
    }

    fn prepare_button_event(
        &mut self,
        event: &MouseEventData,
        pressed: bool,
    ) -> Result<Option<CGEvent>, InputError> {
        let (mouse_type, mouse_button) = match (event.button, pressed) {
            (0, true) => (CGEventType::LeftMouseDown, CGMouseButton::Left),
            (0, false) => (CGEventType::LeftMouseUp, CGMouseButton::Left),
            (1, true) => (CGEventType::OtherMouseDown, CGMouseButton::Center),
            (1, false) => (CGEventType::OtherMouseUp, CGMouseButton::Center),
            (2, true) => (CGEventType::RightMouseDown, CGMouseButton::Right),
            (2, false) => (CGEventType::RightMouseUp, CGMouseButton::Right),
            _ => {
                log::warn!("Unsupported mouse button: {}", event.button);
                return Ok(None);
            }
        };
        let point = self.get_point(event.x, event.y);
        let cg_event =
            CGEvent::new_mouse_event(Self::create_source()?, mouse_type, point, mouse_button)
                .map_err(|()| {
                    InputError::new_custom_error(
                        DeskErrorCode::SYSTEM_ERROR,
                        &format!(
                            "Failed to create mouse {} event for button {}",
                            if pressed { "down" } else { "up" },
                            event.button,
                        ),
                    )
                })?;
        let button = event.button as usize;
        let count = if pressed {
            self.clicks.press(
                button,
                point,
                Instant::now(),
                Duration::from_secs_f64(NSEvent::doubleClickInterval()),
            )
        } else {
            self.clicks.release(button, point)
        };
        // Quartz does not infer multi-click counts for synthetic mouse events.
        // AppKit reads this field on both the press and its matching release.
        cg_event.set_integer_value_field(EventField::MOUSE_EVENT_CLICK_STATE, count);
        Ok(Some(cg_event))
    }
}

// Allow small pointer jitter between clicks in Quartz screen points.
const MULTI_CLICK_DISTANCE: f64 = 4.0;

fn within_click_distance(a: CGPoint, b: CGPoint) -> bool {
    (a.x - b.x).abs() <= MULTI_CLICK_DISTANCE && (a.y - b.y).abs() <= MULTI_CLICK_DISTANCE
}

#[derive(Clone, Copy, Debug)]
struct ClickPress {
    button: usize,
    point: CGPoint,
    at: Instant,
    count: i64,
    released: bool,
}

/// Each remote connection owns its click sequence and matching release counts.
#[derive(Debug, Default)]
struct ClickTracker {
    last_press: Option<ClickPress>,
    pressed_counts: [i64; 3],
}

impl ClickTracker {
    fn press(&mut self, button: usize, point: CGPoint, now: Instant, interval: Duration) -> i64 {
        let count = self
            .last_press
            .filter(|last| {
                last.button == button
                    && last.released
                    && now.duration_since(last.at) <= interval
                    && within_click_distance(last.point, point)
            })
            .map_or(1, |last| last.count.saturating_add(1));
        self.pressed_counts[button] = count;
        self.last_press = Some(ClickPress {
            button,
            point,
            at: now,
            count,
            released: false,
        });
        count
    }

    fn release(&mut self, button: usize, point: CGPoint) -> i64 {
        let count = std::mem::take(&mut self.pressed_counts[button]).max(1);
        if let Some(last) = self.last_press.as_mut() {
            if last.button == button && within_click_distance(last.point, point) {
                last.released = true;
            } else {
                self.last_press = None;
            }
        }
        count
    }

    fn move_to(&mut self, point: CGPoint, buttons: i32) {
        // Hover packets use an unordered channel and may arrive after a press.
        if buttons != 0
            && self
                .last_press
                .is_some_and(|last| !last.released && !within_click_distance(last.point, point))
        {
            self.last_press = None;
        }
    }
}

/// Same translation as the Windows backend's `compute_absolute`, but
/// returning floating-point coordinates because `CGPoint` consumes
/// `f64`. Kept as a free function so unit tests do not need to
/// instantiate a `CGEventSource` (which requires a windowserver
/// connection).
fn compute_absolute_f64(
    left: i32,
    top: i32,
    width: i32,
    height: i32,
    x: f64,
    y: f64,
) -> (f64, f64) {
    let abs_x = left as f64 + x * width as f64;
    let abs_y = top as f64 + y * height as f64;
    (abs_x, abs_y)
}

/// Which kind of motion event to synthesize for a `mousemove`, derived from
/// the DOM `MouseEvent.buttons` bitmask carried on every move event.
///
/// macOS only registers a drag (text selection, drag-and-drop, …) when the
/// motion posted between a button down and its matching up is a `*MouseDragged`
/// event. A plain `MouseMoved` posted while a button is held is treated as a
/// hover, so the drag never happens. We therefore inspect the held-button
/// bitmask on each move and pick the matching dragged variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MoveKind {
    /// No button held — an ordinary hover move.
    Move,
    /// Left (primary) button held — drag with the left button.
    LeftDrag,
    /// Right (secondary) button held — drag with the right button.
    RightDrag,
    /// A non-left/right button held (middle, …) — drag with the center button.
    OtherDrag,
}

/// Map the DOM `MouseEvent.buttons` bitmask (`1` = left, `2` = right,
/// `4` = middle) to the motion kind to inject. Left takes precedence over
/// right, right over other, matching the single-button drag the user
/// perceives when multiple buttons happen to be reported.
fn move_kind_for_buttons(buttons: i32) -> MoveKind {
    const LEFT: i32 = 1;
    const RIGHT: i32 = 2;
    if buttons & LEFT != 0 {
        MoveKind::LeftDrag
    } else if buttons & RIGHT != 0 {
        MoveKind::RightDrag
    } else if buttons != 0 {
        MoveKind::OtherDrag
    } else {
        MoveKind::Move
    }
}

/// Accumulate browser/controller pixel deltas and return the whole pixels that
/// Core Graphics can represent. The fractional remainder is retained for the
/// next event so a high-resolution trackpad cannot starve the remote scroll.
fn take_whole_pixels(accumulator: &mut f64, delta: f64) -> i32 {
    if !delta.is_finite() {
        return 0;
    }
    *accumulator += delta;
    let whole = accumulator.trunc() as i32;
    *accumulator -= whole as f64;
    whole
}

/// Prepare one Core Graphics pixel-scroll event. Axis 2 is only present when
/// horizontal movement is emitted, hence the dynamic wheel count.
fn prepare_pixel_scroll(
    acc_x: &mut f64,
    acc_y: &mut f64,
    delta_x: f64,
    delta_y: f64,
) -> Option<(u32, i32, i32)> {
    let dx = take_whole_pixels(acc_x, delta_x);
    let dy = take_whole_pixels(acc_y, delta_y);
    if dx == 0 && dy == 0 {
        None
    } else {
        Some((if dx == 0 { 1 } else { 2 }, dy, dx))
    }
}

impl MouseEventHandler for MacMouseEventHandler {
    fn handle_mouse_move(&mut self, event: &MouseEventData) -> Result<(), InputError> {
        let point = self.get_point(event.x, event.y);
        self.clicks.move_to(point, event.buttons);
        // While a button is held, macOS needs a *MouseDragged event for the
        // motion to count as a drag; a plain MouseMoved reads as a hover and
        // text selection / drag gestures do nothing. The button argument is
        // only meaningful for OtherMouseDragged but is set correctly for all.
        let (mouse_type, mouse_button) = match move_kind_for_buttons(event.buttons) {
            MoveKind::Move => (CGEventType::MouseMoved, CGMouseButton::Left),
            MoveKind::LeftDrag => (CGEventType::LeftMouseDragged, CGMouseButton::Left),
            MoveKind::RightDrag => (CGEventType::RightMouseDragged, CGMouseButton::Right),
            MoveKind::OtherDrag => (CGEventType::OtherMouseDragged, CGMouseButton::Center),
        };
        let source = Self::create_source()?;
        match CGEvent::new_mouse_event(source, mouse_type, point, mouse_button) {
            Ok(cg_event) => {
                post_remote_input_event(&cg_event);
                Ok(())
            }
            Err(_) => InputError::custom_error(
                DeskErrorCode::SYSTEM_ERROR,
                "Failed to create mouse move event",
            ),
        }
    }

    fn handle_mouse_down(&mut self, event: &MouseEventData) -> Result<(), InputError> {
        if let Some(cg_event) = self.prepare_button_event(event, true)? {
            post_remote_input_event(&cg_event);
        }
        Ok(())
    }

    fn handle_mouse_up(&mut self, event: &MouseEventData) -> Result<(), InputError> {
        if let Some(cg_event) = self.prepare_button_event(event, false)? {
            post_remote_input_event(&cg_event);
        }
        Ok(())
    }

    fn handle_mouse_wheel(&mut self, event: &MouseEventData) -> Result<(), InputError> {
        let Some((wheel_count, dy, dx)) = prepare_pixel_scroll(
            &mut self.wheel_acc_x,
            &mut self.wheel_acc_y,
            event.delta_x,
            event.delta_y,
        ) else {
            return Ok(());
        };

        let source = Self::create_source()?;
        match CGEvent::new_scroll_event(source, ScrollEventUnit::PIXEL, wheel_count, dy, dx, 0) {
            Ok(cg_event) => {
                post_remote_input_event(&cg_event);
                Ok(())
            }
            Err(_) => InputError::custom_error(
                DeskErrorCode::SYSTEM_ERROR,
                "Failed to create scroll event",
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLICK_INTERVAL: Duration = Duration::from_millis(500);

    #[test]
    fn click_counts_match_presses_and_releases_for_single_double_and_triple_clicks() {
        let now = Instant::now();
        let point = CGPoint::new(100.0, 200.0);
        for button in 0..3 {
            let mut clicks = ClickTracker::default();
            for count in 1..=3 {
                let at = now + Duration::from_millis(count as u64 * 100);
                assert_eq!(clicks.press(button, point, at, CLICK_INTERVAL), count);
                assert_eq!(clicks.release(button, point), count);
            }
        }
    }

    #[test]
    fn clicks_follow_the_supplied_system_interval() {
        let now = Instant::now();
        let point = CGPoint::new(100.0, 200.0);
        for (interval, expected) in [(200, 1), (300, 2), (500, 2)] {
            let mut clicks = ClickTracker::default();
            assert_eq!(clicks.press(0, point, now, CLICK_INTERVAL), 1);
            assert_eq!(clicks.release(0, point), 1);
            assert_eq!(
                clicks.press(
                    0,
                    point,
                    now + Duration::from_millis(300),
                    Duration::from_millis(interval),
                ),
                expected,
            );
            assert_eq!(clicks.release(0, point), expected);
        }
    }

    #[test]
    fn clicks_restart_outside_the_position_tolerance() {
        let now = Instant::now();
        let point = CGPoint::new(-100.0, 200.0);
        for (dx, dy, expected) in [(4.0, 4.0, 2), (4.1, 0.0, 1), (0.0, -4.1, 1)] {
            let mut clicks = ClickTracker::default();
            assert_eq!(clicks.press(0, point, now, CLICK_INTERVAL), 1);
            assert_eq!(clicks.release(0, point), 1);
            let next = CGPoint::new(point.x + dx, point.y + dy);
            assert_eq!(
                clicks.press(0, next, now + Duration::from_millis(100), CLICK_INTERVAL),
                expected,
            );
            assert_eq!(clicks.release(0, next), expected);
        }
    }

    #[test]
    fn changing_buttons_restarts_the_click_sequence() {
        let now = Instant::now();
        let point = CGPoint::new(100.0, 200.0);
        let mut clicks = ClickTracker::default();
        for (index, button) in [0, 2, 1, 0].into_iter().enumerate() {
            let at = now + Duration::from_millis(index as u64 * 100);
            assert_eq!(clicks.press(button, point, at, CLICK_INTERVAL), 1);
            assert_eq!(clicks.release(button, point), 1);
        }
    }

    #[test]
    fn releases_preserve_each_held_buttons_count() {
        let now = Instant::now();
        let point = CGPoint::new(100.0, 200.0);
        let mut clicks = ClickTracker::default();
        assert_eq!(clicks.press(0, point, now, CLICK_INTERVAL), 1);
        assert_eq!(clicks.release(0, point), 1);
        assert_eq!(clicks.press(0, point, now, CLICK_INTERVAL), 2);
        assert_eq!(clicks.press(2, point, now, CLICK_INTERVAL), 1);
        assert_eq!(clicks.release(0, point), 2);
        assert_eq!(clicks.release(2, point), 1);
        assert_eq!(clicks.press(0, point, now, CLICK_INTERVAL), 1);
    }

    #[test]
    fn duplicate_presses_and_unmatched_releases_do_not_create_multi_clicks() {
        let now = Instant::now();
        let point = CGPoint::new(100.0, 200.0);
        let mut clicks = ClickTracker::default();
        assert_eq!(clicks.release(0, point), 1);
        assert_eq!(clicks.press(0, point, now, CLICK_INTERVAL), 1);
        assert_eq!(clicks.press(0, point, now, CLICK_INTERVAL), 1);
        assert_eq!(clicks.release(0, point), 1);
    }

    #[test]
    fn moving_away_and_back_breaks_the_sequence_without_changing_the_release() {
        let now = Instant::now();
        let point = CGPoint::new(100.0, 200.0);
        let mut clicks = ClickTracker::default();
        assert_eq!(clicks.press(0, point, now, CLICK_INTERVAL), 1);
        assert_eq!(clicks.release(0, point), 1);
        assert_eq!(clicks.press(0, point, now, CLICK_INTERVAL), 2);
        clicks.move_to(CGPoint::new(150.0, 200.0), 1);
        clicks.move_to(point, 1);
        assert_eq!(clicks.release(0, point), 2);
        assert_eq!(clicks.press(0, point, now, CLICK_INTERVAL), 1);
    }

    #[test]
    fn releasing_at_a_different_position_breaks_the_sequence_without_move_events() {
        let now = Instant::now();
        let point = CGPoint::new(100.0, 200.0);
        let mut clicks = ClickTracker::default();
        assert_eq!(clicks.press(0, point, now, CLICK_INTERVAL), 1);
        assert_eq!(clicks.release(0, CGPoint::new(150.0, 200.0)), 1);
        assert_eq!(clicks.press(0, point, now, CLICK_INTERVAL), 1);
    }

    #[test]
    fn late_hover_packets_do_not_break_a_click_sequence() {
        let now = Instant::now();
        let point = CGPoint::new(100.0, 200.0);
        let mut clicks = ClickTracker::default();
        assert_eq!(clicks.press(0, point, now, CLICK_INTERVAL), 1);
        clicks.move_to(CGPoint::new(150.0, 200.0), 0);
        assert_eq!(clicks.release(0, point), 1);
        clicks.move_to(CGPoint::new(150.0, 200.0), 0);
        assert_eq!(clicks.press(0, point, now, CLICK_INTERVAL), 2);
    }

    #[test]
    fn holding_the_button_past_the_interval_does_not_seed_a_double_click() {
        let now = Instant::now();
        let point = CGPoint::new(100.0, 200.0);
        let mut clicks = ClickTracker::default();
        assert_eq!(clicks.press(0, point, now, CLICK_INTERVAL), 1);
        assert_eq!(clicks.release(0, point), 1);
        assert_eq!(
            clicks.press(0, point, now + Duration::from_secs(1), CLICK_INTERVAL),
            1,
        );
    }

    #[test]
    fn separate_connections_do_not_share_click_history() {
        let now = Instant::now();
        let point = CGPoint::new(100.0, 200.0);
        let mut first = ClickTracker::default();
        let mut second = ClickTracker::default();
        assert_eq!(first.press(0, point, now, CLICK_INTERVAL), 1);
        assert_eq!(first.release(0, point), 1);
        assert_eq!(second.press(0, point, now, CLICK_INTERVAL), 1);
        assert_eq!(first.press(0, point, now, CLICK_INTERVAL), 2);
    }

    #[test]
    #[ignore = "requires a responsive WindowServer session; no input events are posted"]
    fn native_button_events_preserve_click_counts_in_appkit() {
        use crate::model::geometry::{MonitorGeometry, shared};
        use foreign_types::ForeignType;

        for button in 0..3 {
            let mut handler = MacMouseEventHandler::new(shared(MonitorGeometry::FALLBACK)).unwrap();
            let input = MouseEventData {
                button,
                x: 0.5,
                y: 0.5,
                ..MouseEventData::default()
            };
            for expected in 1..=3 {
                for pressed in [true, false] {
                    let event = handler
                        .prepare_button_event(&input, pressed)
                        .unwrap()
                        .unwrap();
                    assert_eq!(
                        event.get_integer_value_field(EventField::MOUSE_EVENT_CLICK_STATE),
                        expected,
                    );
                    // Both bindings refer to the same live, retained CGEvent.
                    let app_event = NSEvent::eventWithCGEvent(unsafe { &*event.as_ptr().cast() })
                        .expect("AppKit must accept the prepared mouse event");
                    assert_eq!(app_event.clickCount() as i64, expected);
                }
            }
        }
    }

    /// No button held resolves to a plain hover move.
    #[test]
    fn move_kind_no_button_is_move() {
        assert_eq!(move_kind_for_buttons(0), MoveKind::Move);
    }

    /// Left button held (DOM buttons bit 1) must become a left drag — this is
    /// the text-selection case that was previously mis-injected as MouseMoved.
    #[test]
    fn move_kind_left_button_is_left_drag() {
        assert_eq!(move_kind_for_buttons(1), MoveKind::LeftDrag);
    }

    /// Right button held (bit 2) becomes a right drag.
    #[test]
    fn move_kind_right_button_is_right_drag() {
        assert_eq!(move_kind_for_buttons(2), MoveKind::RightDrag);
    }

    /// Middle button held (bit 4) becomes an other/center drag.
    #[test]
    fn move_kind_middle_button_is_other_drag() {
        assert_eq!(move_kind_for_buttons(4), MoveKind::OtherDrag);
    }

    /// Left takes precedence when both left and right report as held.
    #[test]
    fn move_kind_left_wins_over_right() {
        assert_eq!(move_kind_for_buttons(1 | 2), MoveKind::LeftDrag);
    }

    /// Right takes precedence over a non-left/right button.
    #[test]
    fn move_kind_right_wins_over_other() {
        assert_eq!(move_kind_for_buttons(2 | 4), MoveKind::RightDrag);
    }

    #[test]
    fn pixel_scroll_does_not_amplify_controller_delta() {
        let (mut acc_x, mut acc_y) = (0.0, 0.0);
        assert_eq!(
            prepare_pixel_scroll(&mut acc_x, &mut acc_y, 0.0, 100.0),
            Some((1, 100, 0))
        );
    }

    #[test]
    fn pixel_scroll_preserves_fractional_trackpad_movement() {
        let (mut acc_x, mut acc_y) = (0.0, 0.0);
        assert_eq!(
            prepare_pixel_scroll(&mut acc_x, &mut acc_y, 0.25, 0.4),
            None
        );
        assert_eq!(
            prepare_pixel_scroll(&mut acc_x, &mut acc_y, 0.25, 0.4),
            None
        );
        assert_eq!(
            prepare_pixel_scroll(&mut acc_x, &mut acc_y, 0.5, 0.4),
            Some((2, 1, 1))
        );
        assert!((acc_y - 0.2).abs() < f64::EPSILON * 4.0);
    }

    #[test]
    fn pixel_scroll_emits_both_axes_when_horizontal_delta_exists() {
        let (mut acc_x, mut acc_y) = (0.0, 0.0);
        assert_eq!(
            prepare_pixel_scroll(&mut acc_x, &mut acc_y, -12.0, 24.0),
            Some((2, 24, -12))
        );
    }

    #[test]
    fn pixel_scroll_drops_non_finite_delta_without_poisoning_accumulator() {
        let (mut acc_x, mut acc_y) = (0.0, 0.0);
        assert_eq!(
            prepare_pixel_scroll(&mut acc_x, &mut acc_y, f64::NAN, f64::INFINITY),
            None
        );
        assert_eq!(
            prepare_pixel_scroll(&mut acc_x, &mut acc_y, 1.0, -1.0),
            Some((2, -1, 1))
        );
    }

    #[test]
    fn compute_absolute_f64_primary_monitor_center() {
        assert_eq!(
            compute_absolute_f64(0, 0, 1280, 800, 0.5, 0.5),
            (640.0, 400.0)
        );
    }

    /// Non-primary display offset to the right. Cursor should land
    /// inside that display, not on the primary.
    #[test]
    fn compute_absolute_f64_offset_display_translates_correctly() {
        let (x, y) = compute_absolute_f64(1280, 0, 1500, 900, 0.5, 0.5);
        assert_eq!((x, y), (1280.0 + 750.0, 0.0 + 450.0));
    }

    /// macOS exposes a negative-Y coordinate space for displays sitting
    /// above the primary, mirrored by macOS Display Settings semantics.
    #[test]
    fn compute_absolute_f64_negative_offset_is_preserved() {
        let (x, y) = compute_absolute_f64(0, -1080, 1920, 1080, 0.25, 0.75);
        assert_eq!((x, y), (480.0, -1080.0 + 810.0));
    }

    /// Hot-update path: the handler reads the shared geometry on every
    /// `get_point` call, so a worker-side mutation through a cloned
    /// handle is visible on the next mouse event. Mirrors the Windows
    /// `compute_absolute_reflects_geometry_update` contract.
    #[test]
    fn compute_absolute_f64_reflects_geometry_update() {
        use crate::model::geometry::{MonitorGeometry, shared};
        let geometry = shared(MonitorGeometry::new(0, 0, 1280, 800));
        let writer = std::sync::Arc::clone(&geometry);

        let (l, t, w, h) = {
            let g = geometry.read().unwrap();
            (g.left, g.top, g.width, g.height)
        };
        assert_eq!(compute_absolute_f64(l, t, w, h, 0.5, 0.5), (640.0, 400.0));

        *writer.write().unwrap() = MonitorGeometry::new(1280, 0, 1500, 900);

        let (l, t, w, h) = {
            let g = geometry.read().unwrap();
            (g.left, g.top, g.width, g.height)
        };
        assert_eq!(
            compute_absolute_f64(l, t, w, h, 0.5, 0.5),
            (1280.0 + 750.0, 450.0)
        );
    }
}
