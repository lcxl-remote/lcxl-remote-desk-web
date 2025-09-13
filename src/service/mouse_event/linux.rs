use crate::{
    desk_error::DeskError,
    model::data_channel::{MouseEventData, MouseEventHandler},
};

pub struct UinputMouseEventHandler {}

impl UinputMouseEventHandler {
    pub fn new(_width: i32, _height: i32) -> Self {
        Self {}
    }
}

impl MouseEventHandler for UinputMouseEventHandler {
    fn handle_mouse_move(&self, event: &MouseEventData) -> Result<(), DeskError> {
        todo!()
    }

    fn handle_mouse_down(&self, event: &MouseEventData) -> Result<(), DeskError> {
        todo!()
    }

    fn handle_mouse_up(&self, event: &MouseEventData) -> Result<(), DeskError> {
        todo!()
    }

    fn handle_mouse_wheel(&self, event: &MouseEventData) -> Result<(), DeskError> {
        todo!()
    }
}
