use windows::Win32::{
    Foundation::GetLastError,
    UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, SendInput, VIRTUAL_KEY,
    },
};

use crate::{
    desk_error::DeskError,
    model::data_channel::{KeyboardEventData, KeyboardEventHandler},
};

pub struct WindowsKeyboardEventHandler {}

impl KeyboardEventHandler for WindowsKeyboardEventHandler {
    fn handle_key_down(&self, event: &KeyboardEventData) -> Result<(), DeskError> {
        let mut input = INPUT::default();
        input.r#type = INPUT_KEYBOARD;
        input.Anonymous.ki.wVk = VIRTUAL_KEY(event.key_code as u16);
        input.Anonymous.ki.dwFlags = KEYBD_EVENT_FLAGS(0);
        let inputs = [input];
        unsafe {
            let result = SendInput(&inputs, size_of::<[INPUT; 1]>() as i32);
            if result == 0 {
                let last_error = GetLastError();
                log::error!(
                    "Failed to send key down event for key code {}, error: {:?}",
                    event.key_code,
                    last_error
                );
            }
        };
        Ok(())
    }

    fn handle_key_up(&self, event: &KeyboardEventData) -> Result<(), DeskError> {
        let mut input = INPUT::default();
        input.r#type = INPUT_KEYBOARD;
        input.Anonymous.ki.wVk = VIRTUAL_KEY(event.key_code as u16);
        input.Anonymous.ki.dwFlags = KEYEVENTF_KEYUP;
        let inputs = [input];
        unsafe {
            let result = SendInput(&inputs, size_of::<[INPUT; 1]>() as i32);
            if result == 0 {
                let last_error = GetLastError();
                log::error!(
                    "Failed to send key up event for key code {}, error: {:?}",
                    event.key_code,
                    last_error
                );
            }
        };
        Ok(())
    }
}
