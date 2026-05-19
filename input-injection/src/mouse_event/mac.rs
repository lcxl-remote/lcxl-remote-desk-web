use crate::{
    error::InputError,
    model::data_channel::{MouseEventData, MouseEventHandler},
};
use core_graphics::event::{
    CGEvent, CGEventTapLocation, CGEventType, CGMouseButton, ScrollEventUnit,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::CGPoint;
use desk_utils::error::DeskErrorCode;

pub struct MacMouseEventHandler {
    /// Global display-space left edge of the captured display.
    /// `CGEvent::post(CGEventTapLocation::HID)` reads `CGPoint`s in the
    /// global multi-display coordinate space, so non-primary displays
    /// require this offset to land the cursor on the right panel.
    left: i32,
    /// Global display-space top edge of the captured display.
    top: i32,
    width: i32,
    height: i32,
}

impl MacMouseEventHandler {
    pub fn new(left: i32, top: i32, width: i32, height: i32) -> Result<Self, InputError> {
        Ok(Self {
            left,
            top,
            width,
            height,
        })
    }

    fn get_point(&self, x: f64, y: f64) -> CGPoint {
        let (px, py) = compute_absolute_f64(self.left, self.top, self.width, self.height, x, y);
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

impl MouseEventHandler for MacMouseEventHandler {
    fn handle_mouse_move(&mut self, event: &MouseEventData) -> Result<(), InputError> {
        let point = self.get_point(event.x, event.y);
        let source = Self::create_source()?;
        match CGEvent::new_mouse_event(source, CGEventType::MouseMoved, point, CGMouseButton::Left)
        {
            Ok(cg_event) => {
                cg_event.post(CGEventTapLocation::HID);
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
                cg_event.post(CGEventTapLocation::HID);
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
                cg_event.post(CGEventTapLocation::HID);
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
                cg_event.post(CGEventTapLocation::HID);
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
}
