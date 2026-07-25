use crate::{
    error::InputError,
    macos_event::post_remote_input_event,
    model::{
        data_channel::{MouseEventData, MouseEventHandler},
        geometry::SharedMonitorGeometry,
    },
};
use core_graphics::event::{CGEvent, CGEventType, CGMouseButton, ScrollEventUnit};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::CGPoint;
use desk_utils::error::DeskErrorCode;

pub struct MacMouseEventHandler {
    /// Shared, hot-updatable display rect for the captured monitor.
    /// `CGEvent::post(CGEventTapLocation::HID)` reads `CGPoint`s in the
    /// global multi-display coordinate space, so non-primary displays
    /// require a non-zero offset to land the cursor on the right panel.
    /// The worker mutates this on display reconfiguration.
    geometry: SharedMonitorGeometry,
}

impl MacMouseEventHandler {
    pub fn new(geometry: SharedMonitorGeometry) -> Result<Self, InputError> {
        Ok(Self { geometry })
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

impl MouseEventHandler for MacMouseEventHandler {
    fn handle_mouse_move(&mut self, event: &MouseEventData) -> Result<(), InputError> {
        let point = self.get_point(event.x, event.y);
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
        let point = self.get_point(event.x, event.y);
        let (mouse_type, mouse_button) = match event.button {
            0 => (CGEventType::LeftMouseDown, CGMouseButton::Left),
            1 => (CGEventType::OtherMouseDown, CGMouseButton::Center),
            2 => (CGEventType::RightMouseDown, CGMouseButton::Right),
            _ => {
                log::warn!("Unsupported mouse button: {}", event.button);
                return Ok(());
            }
        };

        let source = Self::create_source()?;
        match CGEvent::new_mouse_event(source, mouse_type, point, mouse_button) {
            Ok(cg_event) => {
                post_remote_input_event(&cg_event);
                Ok(())
            }
            Err(_) => InputError::custom_error(
                DeskErrorCode::SYSTEM_ERROR,
                &format!(
                    "Failed to create mouse down event for button {}",
                    event.button
                ),
            ),
        }
    }

    fn handle_mouse_up(&mut self, event: &MouseEventData) -> Result<(), InputError> {
        let point = self.get_point(event.x, event.y);
        let (mouse_type, mouse_button) = match event.button {
            0 => (CGEventType::LeftMouseUp, CGMouseButton::Left),
            1 => (CGEventType::OtherMouseUp, CGMouseButton::Center),
            2 => (CGEventType::RightMouseUp, CGMouseButton::Right),
            _ => {
                log::warn!("Unsupported mouse button: {}", event.button);
                return Ok(());
            }
        };

        let source = Self::create_source()?;
        match CGEvent::new_mouse_event(source, mouse_type, point, mouse_button) {
            Ok(cg_event) => {
                post_remote_input_event(&cg_event);
                Ok(())
            }
            Err(_) => InputError::custom_error(
                DeskErrorCode::SYSTEM_ERROR,
                &format!(
                    "Failed to create mouse up event for button {}",
                    event.button
                ),
            ),
        }
    }

    fn handle_mouse_wheel(&mut self, event: &MouseEventData) -> Result<(), InputError> {
        // CoreGraphics scroll event
        // delta_y > 0 means scroll up (content moves down)
        let wheel_count = 1;
        let dy = (event.delta_y * 10.0) as i32;

        let source = Self::create_source()?;
        match CGEvent::new_scroll_event(source, ScrollEventUnit::PIXEL, wheel_count, dy, 0, 0) {
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
