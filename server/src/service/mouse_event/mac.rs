use crate::{
    error::DeskError,
    model::data_channel::{MouseEventData, MouseEventHandler},
};

pub struct MacMouseEventHandler {
    width: i32,
    height: i32,
}

impl MacMouseEventHandler {
    pub fn new(width: i32, height: i32) -> Result<Self, DeskError> {
        Ok(Self { width, height })
    }
}

impl MouseEventHandler for MacMouseEventHandler {
    fn handle_mouse_move(&mut self, _event: &MouseEventData) -> Result<(), DeskError> {
        Ok(())
    }

    fn handle_mouse_down(&mut self, _event: &MouseEventData) -> Result<(), DeskError> {
        Ok(())
    }

    fn handle_mouse_up(&mut self, _event: &MouseEventData) -> Result<(), DeskError> {
        Ok(())
    }

    fn handle_mouse_wheel(&mut self, _event: &MouseEventData) -> Result<(), DeskError> {
        Ok(())
    }
}
