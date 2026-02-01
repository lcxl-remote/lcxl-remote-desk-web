use crate::{
    error::DeskError,
    model::data_channel::{MouseEventData, MouseEventHandler},
};
use core_graphics::event::{
    CGEvent, CGEventTapLocation, CGEventType, CGMouseButton, ScrollEventUnit,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::CGPoint;
use desk_utils::error::DeskErrorCode;

pub struct MacMouseEventHandler {
    width: i32,
    height: i32,
}

impl MacMouseEventHandler {
    pub fn new(width: i32, height: i32) -> Result<Self, DeskError> {
        Ok(Self { width, height })
    }

    fn get_point(&self, x: f64, y: f64) -> CGPoint {
        CGPoint {
            x: x * self.width as f64,
            y: y * self.height as f64,
        }
    }

    fn create_source() -> Result<CGEventSource, DeskError> {
        match CGEventSource::new(CGEventSourceStateID::HIDSystemState) {
            Ok(source) => Ok(source),
            Err(_) => DeskError::custom_error(
                DeskErrorCode::SYSTEM_ERROR,
                "Failed to create event source".to_string(),
            ),
        }
    }
}

impl MouseEventHandler for MacMouseEventHandler {
    fn handle_mouse_move(&mut self, event: &MouseEventData) -> Result<(), DeskError> {
        let point = self.get_point(event.x, event.y);
        let source = Self::create_source()?;
        match CGEvent::new_mouse_event(source, CGEventType::MouseMoved, point, CGMouseButton::Left)
        {
            Ok(cg_event) => {
                cg_event.post(CGEventTapLocation::HID);
                Ok(())
            }
            Err(_) => DeskError::custom_error(
                DeskErrorCode::SYSTEM_ERROR,
                "Failed to create mouse move event".to_string(),
            ),
        }
    }

    fn handle_mouse_down(&mut self, event: &MouseEventData) -> Result<(), DeskError> {
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
            Err(_) => DeskError::custom_error(
                DeskErrorCode::SYSTEM_ERROR,
                format!(
                    "Failed to create mouse down event for button {}",
                    event.button
                ),
            ),
        }
    }

    fn handle_mouse_up(&mut self, event: &MouseEventData) -> Result<(), DeskError> {
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
            Err(_) => DeskError::custom_error(
                DeskErrorCode::SYSTEM_ERROR,
                format!(
                    "Failed to create mouse up event for button {}",
                    event.button
                ),
            ),
        }
    }

    fn handle_mouse_wheel(&mut self, event: &MouseEventData) -> Result<(), DeskError> {
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
            Err(_) => DeskError::custom_error(
                DeskErrorCode::SYSTEM_ERROR,
                "Failed to create scroll event".to_string(),
            ),
        }
    }
}
