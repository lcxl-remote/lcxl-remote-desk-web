use crate::model::data_channel::KeyboardEventHandler;

pub struct UinputKeyboardEventHandler {}

impl KeyboardEventHandler for UinputKeyboardEventHandler {
    fn handle_key_down(
        &self,
        event: &crate::model::data_channel::KeyboardEventData,
    ) -> Result<(), crate::desk_error::DeskError> {
        todo!()
    }

    fn handle_key_up(
        &self,
        event: &crate::model::data_channel::KeyboardEventData,
    ) -> Result<(), crate::desk_error::DeskError> {
        todo!()
    }
}
