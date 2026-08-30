//! Windows-only, single-step raw-input fallback.
//!
//! This adapter never accepts native handles, arbitrary virtual-key codes,
//! modifiers, scripts, or action batches. The caller supplies one fresh
//! Application ObjectRef plus the exact display facts returned by the latest
//! screen observation. Preflight rechecks foreground PID, owner-selected
//! monitor, physical dimensions, and DPI immediately before `SendInput`.

use desk_agent_protocol::computer_use::{
    RawInputAction, RawInputKey, RawInputMouseButton, RawInputStep,
};
use desk_agent_protocol::{AgentError, AgentErrorKind};
use desk_capture_engine::image_capture::monitors::find_monitor_by_device_name;
use desk_input_injection::windows_event::mark_ai_input;
use windows::Win32::Foundation::GetLastError;
use windows::Win32::Graphics::Gdi::{HMONITOR, MONITOR_DEFAULTTONULL, MonitorFromWindow};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_KEYBOARD, INPUT_MOUSE, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, MOUSEEVENTF_ABSOLUTE,
    MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MOVE,
    MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL,
    SendInput, VIRTUAL_KEY, VK_BACK, VK_DELETE, VK_DOWN, VK_END, VK_ESCAPE, VK_HOME, VK_LEFT,
    VK_NEXT, VK_PRIOR, VK_RETURN, VK_RIGHT, VK_SPACE, VK_TAB, VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetSystemMetrics, GetWindowThreadProcessId, SM_CXVIRTUALSCREEN,
    SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RawInputPreflight {
    left: i32,
    top: i32,
    width: u32,
    height: u32,
}

pub(super) fn preflight(
    expected_process_id: u32,
    expected_process_started_at: u64,
    selected_display: &str,
    action: &RawInputAction,
) -> Result<RawInputPreflight, AgentError> {
    if selected_display.is_empty() || !selected_display.eq_ignore_ascii_case(&action.screen.display)
    {
        return Err(failure(
            AgentErrorKind::PermissionDenied,
            "raw input requires the exact current owner-selected display",
            false,
        ));
    }
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.0.is_null() {
        return Err(failure(
            AgentErrorKind::SessionUnavailable,
            "raw input requires a visible foreground application",
            true,
        ));
    }
    let mut process_id = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut process_id)) };
    if process_id == 0 || process_id != expected_process_id {
        return Err(failure(
            AgentErrorKind::InvalidInput,
            "foreground application identity changed after observation",
            false,
        ));
    }
    if super::windows_uia_observer::process_start(process_id) != Some(expected_process_started_at) {
        return Err(failure(
            AgentErrorKind::InvalidInput,
            "foreground application process incarnation changed after observation",
            false,
        ));
    }

    let monitor = find_monitor_by_device_name(selected_display)
        .map_err(|error| failure(AgentErrorKind::SessionUnavailable, &error.to_string(), true))?
        .ok_or_else(|| {
            failure(
                AgentErrorKind::SessionUnavailable,
                "the owner-selected display is no longer attached",
                true,
            )
        })?;
    let foreground_monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONULL) };
    if foreground_monitor == HMONITOR::default()
        || foreground_monitor.0 as isize != monitor.hmonitor_raw
    {
        return Err(failure(
            AgentErrorKind::InvalidInput,
            "foreground application is no longer on the observed display",
            false,
        ));
    }
    let width = monitor.rect.right.saturating_sub(monitor.rect.left) as u32;
    let height = monitor.rect.bottom.saturating_sub(monitor.rect.top) as u32;
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    let dpi = if dpi == 0 { 96 } else { dpi };
    if width != action.screen.width
        || height != action.screen.height
        || dpi != action.screen.dpi_x
        || dpi != action.screen.dpi_y
    {
        return Err(failure(
            AgentErrorKind::InvalidInput,
            "display geometry or DPI changed after the screen observation",
            false,
        ));
    }
    Ok(RawInputPreflight {
        left: monitor.rect.left,
        top: monitor.rect.top,
        width,
        height,
    })
}

pub(super) fn apply(
    expected_process_id: u32,
    expected_process_started_at: u64,
    selected_display: &str,
    action: &RawInputAction,
) -> Result<String, AgentError> {
    let screen = preflight(
        expected_process_id,
        expected_process_started_at,
        selected_display,
        action,
    )?;
    let inputs = build_inputs(&screen, &action.step)?;
    let expected = inputs.len() as u32;
    let sent = unsafe { SendInput(&inputs, size_of::<INPUT>() as i32) };
    if sent != expected {
        let code = unsafe { GetLastError().0 };
        return Err(failure(
            AgentErrorKind::Internal,
            &format!("SendInput accepted {sent}/{expected} events; GetLastError={code}"),
            true,
        ));
    }
    // This is a fresh post-action observation, not semantic verification. It
    // proves only that the exact foreground target and coordinate space still
    // exist; the orchestrator must observe UI/screen state again before
    // deciding that the requested business outcome occurred.
    preflight(
        expected_process_id,
        expected_process_started_at,
        selected_display,
        action,
    )?;
    Ok(format!(
        "one {} raw-input fallback step was injected; foreground/display/DPI remained current but application state is unverified",
        step_name(&action.step)
    ))
}

fn build_inputs(screen: &RawInputPreflight, step: &RawInputStep) -> Result<Vec<INPUT>, AgentError> {
    match step {
        RawInputStep::Click { x, y, button } => {
            if *x >= screen.width || *y >= screen.height {
                return Err(failure(
                    AgentErrorKind::InvalidInput,
                    "raw input click is outside the current display",
                    false,
                ));
            }
            let (nx, ny) = absolute_normalized(screen.left + *x as i32, screen.top + *y as i32);
            let mut movement = INPUT::default();
            movement.r#type = INPUT_MOUSE;
            movement.Anonymous.mi.dx = nx;
            movement.Anonymous.mi.dy = ny;
            movement.Anonymous.mi.dwFlags =
                MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK;
            mark_ai_input(&mut movement);

            let (down_flag, up_flag) = match button {
                RawInputMouseButton::Primary => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
                RawInputMouseButton::Secondary => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
            };
            let mut down = INPUT::default();
            down.r#type = INPUT_MOUSE;
            down.Anonymous.mi.dwFlags = down_flag;
            mark_ai_input(&mut down);
            let mut up = INPUT::default();
            up.r#type = INPUT_MOUSE;
            up.Anonymous.mi.dwFlags = up_flag;
            mark_ai_input(&mut up);
            Ok(vec![movement, down, up])
        }
        RawInputStep::KeyPress { key } => {
            let virtual_key = raw_key(*key);
            let mut down = INPUT::default();
            down.r#type = INPUT_KEYBOARD;
            down.Anonymous.ki.wVk = virtual_key;
            mark_ai_input(&mut down);
            let mut up = down;
            up.Anonymous.ki.dwFlags = KEYEVENTF_KEYUP;
            mark_ai_input(&mut up);
            Ok(vec![down, up])
        }
        RawInputStep::TypeText { text } => {
            let mut inputs = Vec::with_capacity(text.encode_utf16().count() * 2);
            for unit in text.encode_utf16() {
                let mut down = INPUT::default();
                down.r#type = INPUT_KEYBOARD;
                down.Anonymous.ki.wScan = unit;
                down.Anonymous.ki.dwFlags = KEYEVENTF_UNICODE;
                mark_ai_input(&mut down);
                let mut up = down;
                up.Anonymous.ki.dwFlags = KEYEVENTF_UNICODE | KEYEVENTF_KEYUP;
                mark_ai_input(&mut up);
                inputs.extend([down, up]);
            }
            Ok(inputs)
        }
        RawInputStep::Scroll {
            horizontal,
            vertical,
        } => {
            let mut inputs = Vec::with_capacity(2);
            if *vertical != 0 {
                let mut input = INPUT::default();
                input.r#type = INPUT_MOUSE;
                input.Anonymous.mi.dwFlags = MOUSEEVENTF_WHEEL;
                input.Anonymous.mi.mouseData = *vertical as u32;
                mark_ai_input(&mut input);
                inputs.push(input);
            }
            if *horizontal != 0 {
                let mut input = INPUT::default();
                input.r#type = INPUT_MOUSE;
                input.Anonymous.mi.dwFlags = MOUSEEVENTF_HWHEEL;
                input.Anonymous.mi.mouseData = *horizontal as u32;
                mark_ai_input(&mut input);
                inputs.push(input);
            }
            Ok(inputs)
        }
    }
}

fn absolute_normalized(x: i32, y: i32) -> (i32, i32) {
    let (left, top, width, height) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    absolute_normalized_for_virtual_desktop(x, y, left, top, width, height)
}

fn absolute_normalized_for_virtual_desktop(
    x: i32,
    y: i32,
    left: i32,
    top: i32,
    width: i32,
    height: i32,
) -> (i32, i32) {
    let span_x = (width - 1).max(1) as i64;
    let span_y = (height - 1).max(1) as i64;
    let nx = ((x - left) as i64 * 65_535 + span_x / 2) / span_x;
    let ny = ((y - top) as i64 * 65_535 + span_y / 2) / span_y;
    (nx.clamp(0, 65_535) as i32, ny.clamp(0, 65_535) as i32)
}

fn raw_key(key: RawInputKey) -> VIRTUAL_KEY {
    match key {
        RawInputKey::Enter => VK_RETURN,
        RawInputKey::Tab => VK_TAB,
        RawInputKey::Escape => VK_ESCAPE,
        RawInputKey::Backspace => VK_BACK,
        RawInputKey::Delete => VK_DELETE,
        RawInputKey::Space => VK_SPACE,
        RawInputKey::ArrowUp => VK_UP,
        RawInputKey::ArrowDown => VK_DOWN,
        RawInputKey::ArrowLeft => VK_LEFT,
        RawInputKey::ArrowRight => VK_RIGHT,
        RawInputKey::Home => VK_HOME,
        RawInputKey::End => VK_END,
        RawInputKey::PageUp => VK_PRIOR,
        RawInputKey::PageDown => VK_NEXT,
    }
}

fn step_name(step: &RawInputStep) -> &'static str {
    match step {
        RawInputStep::Click { .. } => "click",
        RawInputStep::KeyPress { .. } => "key",
        RawInputStep::TypeText { .. } => "type-text",
        RawInputStep::Scroll { .. } => "scroll",
    }
}

fn failure(kind: AgentErrorKind, message: &str, retryable: bool) -> AgentError {
    AgentError {
        kind,
        message: message.to_string(),
        retryable,
        safe_for_model: true,
        error_code: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negative_virtual_desktop_origin_maps_exactly() {
        assert_eq!(
            absolute_normalized_for_virtual_desktop(-1920, 0, -1920, 0, 3840, 1080),
            (0, 0)
        );
        assert_eq!(
            absolute_normalized_for_virtual_desktop(1919, 1079, -1920, 0, 3840, 1080),
            (65_535, 65_535)
        );
    }

    #[test]
    fn typed_text_uses_ai_marked_unicode_pairs() {
        let screen = RawInputPreflight {
            left: 0,
            top: 0,
            width: 1920,
            height: 1080,
        };
        let inputs = build_inputs(
            &screen,
            &RawInputStep::TypeText {
                text: "A中".into()
            },
        )
        .expect("bounded text");
        assert_eq!(inputs.len(), 4);
        assert!(inputs.iter().all(|input| unsafe {
            input.Anonymous.ki.dwExtraInfo == desk_input_injection::windows_event::AI_INPUT_MARKER
        }));
    }

    /// Run only from a production SessionWorker attached to the visible test
    /// desktop. The exact foreground PID/display/geometry/DPI are supplied by
    /// the harness after the owner opens the fixture application. Escape is a
    /// bounded, non-text key used to prove the real marked SendInput path.
    #[test]
    #[ignore = "requires a visible interactive Windows desktop and explicit fixture environment"]
    fn live_foreground_fixture_executes_one_marked_step_and_reobserves() {
        let process_id = std::env::var("LRD_RAW_INPUT_TEST_FOREGROUND_PID")
            .expect("foreground PID")
            .parse::<u32>()
            .expect("numeric PID");
        let display = std::env::var("LRD_RAW_INPUT_TEST_DISPLAY").expect("selected display");
        let width = std::env::var("LRD_RAW_INPUT_TEST_WIDTH")
            .expect("display width")
            .parse::<u32>()
            .expect("numeric width");
        let height = std::env::var("LRD_RAW_INPUT_TEST_HEIGHT")
            .expect("display height")
            .parse::<u32>()
            .expect("numeric height");
        let dpi = std::env::var("LRD_RAW_INPUT_TEST_DPI")
            .expect("foreground DPI")
            .parse::<u32>()
            .expect("numeric DPI");
        let action = RawInputAction {
            screen: desk_agent_protocol::computer_use::RawInputScreenContext {
                display: display.clone(),
                width,
                height,
                dpi_x: dpi,
                dpi_y: dpi,
            },
            step: RawInputStep::KeyPress {
                key: RawInputKey::Escape,
            },
        };
        let process_started_at = super::super::windows_uia_observer::process_start(process_id)
            .expect("foreground process start identity");
        let summary = apply(process_id, process_started_at, &display, &action)
            .expect("one live raw input step");
        assert!(summary.contains("unverified"));
    }
}
