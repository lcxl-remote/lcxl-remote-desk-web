use evdev::{KeyCode, KeyEvent, uinput::VirtualDevice};

use crate::{
    error::DeskError,
    model::data_channel::{KeyboardEventData, KeyboardEventHandler},
};

pub struct UinputKeyboardEventHandler {
    pub virtual_device: VirtualDevice,
}

impl UinputKeyboardEventHandler {
    pub fn new() -> Result<Self, DeskError> {
        // https://www.kernel.org/doc/html/v4.12/input/uinput.html
        let mut keys = evdev::AttributeSet::<KeyCode>::new();
        for code in 1..255 {
            keys.insert(KeyCode(code));
        }
        let virtual_device = VirtualDevice::builder()?
            .name("lcxl-web-remote-desk-keyboard")
            .with_keys(&keys)?
            .build()?;
        Ok(Self { virtual_device })
    }
}

impl KeyboardEventHandler for UinputKeyboardEventHandler {
    fn handle_key_down(&mut self, event: &KeyboardEventData) -> Result<(), DeskError> {
        let down_event = *KeyEvent::new(KeyCode(event.key_code as u16), 1);
        self.virtual_device.emit(&[down_event])?;
        Ok(())
    }

    fn handle_key_up(&mut self, event: &KeyboardEventData) -> Result<(), DeskError> {
        let up_event = *KeyEvent::new(KeyCode(event.key_code as u16), 0);
        self.virtual_device.emit(&[up_event])?;
        Ok(())
    }
}
