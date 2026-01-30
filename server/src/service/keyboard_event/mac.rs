use crate::{
    error::DeskError,
    model::data_channel::{KeyboardEventData, KeyboardEventHandler},
};

pub struct MacKeyboardEventHandler {}

impl KeyboardEventHandler for MacKeyboardEventHandler {
    fn handle_key_down(&mut self, _event: &KeyboardEventData) -> Result<(), DeskError> {
        Ok(())
    }

    fn handle_key_up(&mut self, _event: &KeyboardEventData) -> Result<(), DeskError> {
        Ok(())
    }
}
