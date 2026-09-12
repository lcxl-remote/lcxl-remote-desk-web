//! PID-only input. Experimental window-local routing is isolated here.
use core_graphics::{
    event::{CGEvent, CGEventFlags, CGEventType, EventField, ScrollEventUnit},
    event_source::{CGEventSource, CGEventSourceStateID},
    geometry::CGPoint,
};
use desk_agent_protocol::{
    AgentError, AgentErrorKind,
    background_input::{BackgroundInputAction, InputModifier, WindowInputGeometry, key_code},
};
use foreign_types::ForeignType;
use objc2::{class, msg_send, rc::autoreleasepool, runtime::AnyObject};
use objc2_foundation::NSPoint;
use std::{ffi::c_void, sync::OnceLock, time::Duration};

pub(super) struct Target {
    pub pid: u32,
    pub window_id: u32,
    pub origin: CGPoint,
    pub width: f64,
    pub height: f64,
    pub element_point: Option<CGPoint>,
}
// Match AppKit's CGEventRef encoding, rather than void*, so objc2's debug
// ABI validation checks the actual Objective-C signature.
#[repr(C)]
struct NativeEvent {
    _private: [u8; 0],
}
unsafe impl objc2::encode::RefEncode for NativeEvent {
    const ENCODING_REF: objc2::encode::Encoding =
        objc2::encode::Encoding::Pointer(&objc2::encode::Encoding::Struct("__CGEvent", &[]));
}
type WindowLocation = unsafe extern "C" fn(*const c_void, CGPoint);
fn window_location() -> Option<WindowLocation> {
    static SYMBOL: OnceLock<Option<WindowLocation>> = OnceLock::new();
    *SYMBOL.get_or_init(|| unsafe {
        let p = libc::dlsym(libc::RTLD_DEFAULT, c"CGEventSetWindowLocation".as_ptr());
        (!p.is_null()).then(|| std::mem::transmute::<*mut c_void, WindowLocation>(p))
    })
}
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventSetLocation(event: *const c_void, point: CGPoint);
    fn CGPreflightPostEventAccess() -> bool;
    fn CGEventPostToPid(pid: i32, event: *const c_void);
}
pub(super) fn keyboard_ready() -> bool {
    unsafe { CGPreflightPostEventAccess() }
}
pub(super) fn mouse_ready() -> bool {
    keyboard_ready() && window_location().is_some()
}
fn failure(message: impl Into<String>) -> AgentError {
    AgentError {
        kind: AgentErrorKind::InvalidInput,
        message: message.into(),
        retryable: false,
        safe_for_model: true,
        error_code: None,
    }
}
fn source() -> Result<CGEventSource, AgentError> {
    CGEventSource::new(CGEventSourceStateID::Private)
        .map_err(|_| failure("Cannot create a macOS input event source"))
}
fn key(
    source: &CGEventSource,
    code: u16,
    down: bool,
    flags: CGEventFlags,
) -> Result<CGEvent, AgentError> {
    let e = CGEvent::new_keyboard_event(source.clone(), code, down)
        .map_err(|_| failure("Cannot create keyboard event"))?;
    e.set_flags(flags);
    Ok(e)
}
fn flags(modifiers: &[InputModifier]) -> CGEventFlags {
    let mut f = CGEventFlags::empty();
    for m in modifiers {
        f |= match m {
            InputModifier::Command => CGEventFlags::CGEventFlagCommand,
            InputModifier::Control => CGEventFlags::CGEventFlagControl,
            InputModifier::Option => CGEventFlags::CGEventFlagAlternate,
            InputModifier::Shift => CGEventFlags::CGEventFlagShift,
        };
    }
    f
}
fn post(event: &CGEvent, pid: u32) {
    unsafe {
        CGEventPostToPid(pid as i32, event.as_ptr().cast());
    }
}
fn mouse_event(
    kind: CGEventType,
    target: &Target,
    point: CGPoint,
    count: i64,
) -> Result<CGEvent, AgentError> {
    autoreleasepool(|_| unsafe {
        let process: *mut AnyObject = msg_send![class!(NSProcessInfo), processInfo];
        let uptime: f64 = msg_send![process, systemUptime];
        let event: *mut AnyObject = msg_send![class!(NSEvent),mouseEventWithType:kind as usize location:NSPoint::new(point.x,point.y) modifierFlags:1usize<<20 timestamp:uptime windowNumber:target.window_id as isize context:std::ptr::null::<AnyObject>() eventNumber:1isize clickCount:count as isize pressure:1.0f32];
        if event.is_null() {
            return Err(failure("Cannot construct a window mouse event"));
        }
        let raw: *mut NativeEvent = msg_send![event, CGEvent];
        if raw.is_null() {
            return Err(failure("NSEvent returned no CGEvent"));
        }
        let event = CGEvent::from_ptr(
            core_foundation::base::CFRetain(raw.cast())
                .cast_mut()
                .cast(),
        );
        CGEventSetLocation(event.as_ptr().cast(), point);
        event.set_integer_value_field(
            EventField::MOUSE_EVENT_WINDOW_UNDER_MOUSE_POINTER,
            target.window_id as i64,
        );
        event.set_integer_value_field(
            EventField::MOUSE_EVENT_WINDOW_UNDER_MOUSE_POINTER_THAT_CAN_HANDLE_THIS_EVENT,
            target.window_id as i64,
        );
        event.set_integer_value_field(EventField::MOUSE_EVENT_SUB_TYPE, 3);
        event.set_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER, 0);
        window_location().ok_or_else(|| {
            failure("Background mouse window routing is unavailable on this macOS version")
        })?(
            event.as_ptr().cast(),
            CGPoint::new(point.x - target.origin.x, point.y - target.origin.y),
        );
        Ok(event)
    })
}
// Screenshot pixels map to Quartz points only at the native input boundary.
fn pixel_axis(origin: f64, size: f64, pixels: u32, value: u32) -> Result<f64, AgentError> {
    if pixels == 0 || value >= pixels {
        return Err(failure(
            "Mouse position is outside the screenshot pixel dimensions; use 0 <= x < width and 0 <= y < height",
        ));
    }
    Ok(origin + size * f64::from(value) / f64::from(pixels))
}

pub(super) fn validate(
    target: &Target,
    input: &BackgroundInputAction,
    geometry: Option<&WindowInputGeometry>,
) -> Result<Option<CGPoint>, AgentError> {
    input.validate().map_err(failure)?;
    if !keyboard_ready() {
        return Err(failure("macOS event-posting permission is missing"));
    }
    let Some((position, _)) = input.locator() else {
        return Ok(None);
    };
    if !mouse_ready() {
        return Err(failure(
            "Background mouse routing is unavailable; keyboard and semantic UI remain separate capabilities",
        ));
    }
    let point = if let Some(p) = position {
        let g=geometry.ok_or_else(|| failure("Read this window's screenshot before coordinate input, or use an observed element_id"))?;
        if g.width_millipoints != (target.width * 1000.0).round() as u64
            || g.height_millipoints != (target.height * 1000.0).round() as u64
        {
            return Err(failure(
                "Window size changed since the screenshot; capture it again before clicking",
            ));
        }
        CGPoint::new(
            pixel_axis(target.origin.x, target.width, g.width_pixels, p.x)?,
            pixel_axis(target.origin.y, target.height, g.height_pixels, p.y)?,
        )
    } else {
        target
            .element_point
            .ok_or_else(|| failure("Mouse element has no current position"))?
    };
    if !point.x.is_finite()
        || !point.y.is_finite()
        || point.x < target.origin.x
        || point.y < target.origin.y
        || point.x >= target.origin.x + target.width
        || point.y >= target.origin.y + target.height
    {
        return Err(failure("Mouse position is outside the current window"));
    }
    Ok(Some(point))
}
/// The caller checks cancellation between events; a started down/up pair is always released.
pub(super) fn apply(
    target: &Target,
    input: &BackgroundInputAction,
    geometry: Option<&WindowInputGeometry>,
    check: impl Fn() -> Result<(), AgentError>,
) -> Result<usize, AgentError> {
    let point = validate(target, input, geometry)?;
    tracing::debug!(pid=target.pid, window_id=target.window_id, action=?input.kind(), "dispatching macOS background input");
    let source = source()?;
    let sent = std::cell::Cell::new(0usize);
    let pair = |down: CGEvent, up: CGEvent| -> Result<(), AgentError> {
        check()?;
        post(&down, target.pid);
        sent.set(sent.get() + 1);
        std::thread::sleep(Duration::from_millis(20));
        post(&up, target.pid);
        sent.set(sent.get() + 1);
        Ok(())
    };
    let result = (|| {
        match input {
            BackgroundInputAction::TypeText { text } => {
                for ch in text.chars() {
                    let down = key(&source, 0, true, CGEventFlags::empty())?;
                    let up = key(&source, 0, false, CGEventFlags::empty())?;
                    let text = ch.to_string();
                    down.set_string(&text);
                    up.set_string(&text);
                    pair(down, up)?;
                }
            }
            BackgroundInputAction::KeyPress {
                key: name,
                modifiers,
            } => pair(
                key(&source, key_code(name).unwrap(), true, flags(modifiers))?,
                key(
                    &source,
                    key_code(name).unwrap(),
                    false,
                    CGEventFlags::empty(),
                )?,
            )?,
            BackgroundInputAction::Click { .. } | BackgroundInputAction::DoubleClick { .. } => {
                for count in 1..=if matches!(input, BackgroundInputAction::DoubleClick { .. }) {
                    2
                } else {
                    1
                } {
                    pair(
                        mouse_event(CGEventType::LeftMouseDown, target, point.unwrap(), count)?,
                        mouse_event(CGEventType::LeftMouseUp, target, point.unwrap(), count)?,
                    )?;
                }
            }
            BackgroundInputAction::Scroll {
                horizontal,
                vertical,
                ..
            } => {
                check()?;
                let p = point.unwrap();
                let moved = mouse_event(CGEventType::MouseMoved, target, p, 0)?;
                post(&moved, target.pid);
                sent.set(sent.get() + 1);
                std::thread::sleep(Duration::from_millis(100));
                let event = CGEvent::new_scroll_event(
                    source.clone(),
                    ScrollEventUnit::PIXEL,
                    2,
                    *vertical,
                    *horizontal,
                    0,
                )
                .map_err(|_| failure("Cannot create scroll event"))?;
                unsafe {
                    CGEventSetLocation(event.as_ptr().cast(), p);
                }
                event.set_flags(CGEventFlags::CGEventFlagCommand);
                for field in [
                    EventField::MOUSE_EVENT_WINDOW_UNDER_MOUSE_POINTER,
                    EventField::MOUSE_EVENT_WINDOW_UNDER_MOUSE_POINTER_THAT_CAN_HANDLE_THIS_EVENT,
                    51,
                ] {
                    event.set_integer_value_field(field, target.window_id as i64);
                }
                unsafe {
                    window_location().unwrap()(
                        event.as_ptr().cast(),
                        CGPoint::new(p.x - target.origin.x, p.y - target.origin.y),
                    );
                }
                check()?;
                post(&event, target.pid);
                sent.set(sent.get() + 1);
            }
        }
        Ok(())
    })();
    let sent = sent.get();
    result.map_err(|mut e:AgentError|{e.message=format!("{}; {sent} input events dispatched. Read the current UI before deciding the next action.",e.message);e})?;
    Ok(sent)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn screenshot_pixels_convert_without_normalized_coordinate_assumptions() {
        assert_eq!(pixel_axis(-1000.0, 640.0, 640, 0).unwrap(), -1000.0);
        assert_eq!(pixel_axis(-1000.0, 640.0, 640, 500).unwrap(), -500.0);
        assert_eq!(pixel_axis(200.0, 935.0, 935, 85).unwrap(), 285.0);
        assert_eq!(pixel_axis(200.0, 640.0, 1280, 1000).unwrap(), 700.0);
        assert!(pixel_axis(0.0, 640.0, 640, 640).is_err());
        assert!(pixel_axis(0.0, 640.0, 0, 0).is_err());
    }
}
