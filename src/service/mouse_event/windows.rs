use windows::Win32::UI::{
    Input::KeyboardAndMouse::{
        INPUT, INPUT_MOUSE, MOUSE_EVENT_FLAGS, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
        MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
        MOUSEEVENTF_WHEEL, SendInput,
    },
    WindowsAndMessaging::SetCursorPos,
};

use crate::{
    desk_error::DeskError,
    model::data_channel::{MouseEventData, MouseEventHandler},
};

pub struct WindowsMouseEventHandler {
    pub width: i32,
    pub height: i32,
}

impl WindowsMouseEventHandler {
    pub fn new(width: i32, height: i32) -> Self {
        //let width = unsafe { GetSystemMetrics(SM_CXSCREEN) };
        //let height = unsafe { GetSystemMetrics(SM_CYSCREEN) };
        Self { width, height }
    }
}

impl MouseEventHandler for WindowsMouseEventHandler {
    fn handle_mouse_move(&self, event: &MouseEventData) -> Result<(), DeskError> {
        let x = (event.x * self.width as f64) as i32;
        let y = (event.y * self.height as f64) as i32;
        let result = unsafe { SetCursorPos(x, y) };
        if !result.is_err() {
            log::error!(
                "Failed to set cursor position to ({}, {}), error: {:?}",
                x,
                y,
                result
            );
        }
        Ok(())
    }

    fn handle_mouse_down(
        &self,
        event: &MouseEventData,
    ) -> Result<(), crate::desk_error::DeskError> {
        let mut mouse_event_flags = MOUSE_EVENT_FLAGS(0);
        match event.button {
            0 => mouse_event_flags |= MOUSEEVENTF_LEFTDOWN,
            1 => mouse_event_flags |= MOUSEEVENTF_MIDDLEDOWN,
            2 => mouse_event_flags |= MOUSEEVENTF_RIGHTDOWN,
            _ => {
                log::warn!("Unsupported mouse button: {}", event.button);
                return Ok(());
            }
        };
        let mut input = INPUT::default();
        input.r#type = INPUT_MOUSE;
        input.Anonymous.mi.dwFlags = mouse_event_flags;
        let inputs = [input];
        unsafe {
            SendInput(&inputs, size_of::<[INPUT; 1]>() as i32);
            //windows::Win32::UI::Input::KeyboardAndMouse::mouse_event(mouse_event_flags, x, y, 0, 0)
        };
        Ok(())
    }

    fn handle_mouse_up(&self, event: &MouseEventData) -> Result<(), DeskError> {
        let mut mouse_event_flags = MOUSE_EVENT_FLAGS(0);
        match event.button {
            0 => mouse_event_flags |= MOUSEEVENTF_LEFTUP,
            1 => mouse_event_flags |= MOUSEEVENTF_MIDDLEUP,
            2 => mouse_event_flags |= MOUSEEVENTF_RIGHTUP,
            _ => {
                log::warn!("Unsupported mouse button: {}", event.button);
                return Ok(());
            }
        };
        let mut input = INPUT::default();
        input.r#type = INPUT_MOUSE;
        input.Anonymous.mi.dwFlags = mouse_event_flags;
        let inputs = [input];
        unsafe {
            SendInput(&inputs, size_of::<[INPUT; 1]>() as i32);
            //windows::Win32::UI::Input::KeyboardAndMouse::mouse_event(mouse_event_flags, x, y, 0, 0)
        };
        Ok(())
    }

    fn handle_mouse_wheel(&self, event: &MouseEventData) -> Result<(), DeskError> {
        let mut mouse_event_flags = MOUSE_EVENT_FLAGS(0);
        mouse_event_flags |= MOUSEEVENTF_WHEEL;
        let wheel_delta = (event.delta_y * 120.0) as i32;
        let mut input = INPUT::default();
        input.r#type = INPUT_MOUSE;
        input.Anonymous.mi.dwFlags = mouse_event_flags;
        input.Anonymous.mi.mouseData = wheel_delta as u32;
        let inputs = [input];
        unsafe {
            SendInput(&inputs, size_of::<[INPUT; 1]>() as i32);
        };
        Ok(())
    }
}
