use evdev::{KeyCode, KeyEvent, uinput::VirtualDevice};

use crate::{
    error::InputError,
    linux_display::UINPUT_KEYBOARD_DEVICE_NAME,
    model::data_channel::{KeyboardEventData, KeyboardEventHandler},
};

use super::keyboard_event_to_evdev;

pub struct UinputKeyboardEventHandler {
    pub virtual_device: VirtualDevice,
}

impl UinputKeyboardEventHandler {
    pub fn new() -> Result<Self, InputError> {
        // https://www.kernel.org/doc/html/v4.12/input/uinput.html
        let mut keys = evdev::AttributeSet::<KeyCode>::new();
        for code in 1..255 {
            keys.insert(KeyCode(code));
        }
        let virtual_device = VirtualDevice::builder()?
            .name(UINPUT_KEYBOARD_DEVICE_NAME)
            .input_id(evdev::InputId::new(
                evdev::BusType::BUS_USB,
                0x1234,
                0x5679,
                0x111,
            ))
            .with_keys(&keys)?
            .build()?;
        Ok(Self { virtual_device })
    }
}

impl KeyboardEventHandler for UinputKeyboardEventHandler {
    fn handle_key_down(&mut self, event: &KeyboardEventData) -> Result<(), InputError> {
        if let Some(evdev_code) = keyboard_event_to_evdev(event) {
            let down_event = *KeyEvent::new(KeyCode(evdev_code), 1);
            let syn_event = evdev::InputEvent::new(
                evdev::EventType::SYNCHRONIZATION.0,
                evdev::SynchronizationCode::SYN_REPORT.0,
                0,
            );
            let result = self.virtual_device.emit(&[down_event, syn_event]);
            if let Err(e) = result {
                log::error!("Failed to emit key down event: {}", e);
                return Err(InputError::from(e));
            } else {
                log::debug!("Key down emitted, code: {} -> {}", event.code, evdev_code);
            }
        }
        Ok(())
    }

    fn handle_key_up(&mut self, event: &KeyboardEventData) -> Result<(), InputError> {
        if let Some(evdev_code) = keyboard_event_to_evdev(event) {
            let up_event = *KeyEvent::new(KeyCode(evdev_code), 0);
            let syn_event = evdev::InputEvent::new(
                evdev::EventType::SYNCHRONIZATION.0,
                evdev::SynchronizationCode::SYN_REPORT.0,
                0,
            );
            let result = self.virtual_device.emit(&[up_event, syn_event]);
            if let Err(e) = result {
                log::error!("Failed to emit key up event: {}", e);
                return Err(InputError::from(e));
            } else {
                log::debug!("Key up emitted, code: {} -> {}", event.code, evdev_code);
            }
        }
        Ok(())
    }
}
