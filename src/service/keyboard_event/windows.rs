use windows::Win32::UI::Input::KeyboardAndMouse::{INPUT, INPUT_KEYBOARD};

use crate::{
    desk_error::DeskError,
    model::data_channel::{KeyboardEventData, KeyboardEventHandler},
};

pub struct WindowsKeyboardEventHandler {}

impl KeyboardEventHandler for WindowsKeyboardEventHandler {
    fn handle_key_down(&self, event: &KeyboardEventData) -> Result<(), DeskError> {
        let mut input = INPUT::default();
        input.r#type = INPUT_KEYBOARD;
        //input.Anonymous.ki.wVk = event.key_code as u16;
        Ok(())
    }

    fn handle_key_up(&self, event: &KeyboardEventData) -> Result<(), DeskError> {
        //windows::send_key_event(event, false)

        Ok(())
    }
}
