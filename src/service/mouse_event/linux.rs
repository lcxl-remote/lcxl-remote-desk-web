use evdev::{
    AbsInfo, AbsoluteAxisCode, AttributeSet, AttributeSetRef, EventType, InputEvent, KeyCode,
    KeyEvent, UinputAbsSetup, uinput::VirtualDevice,
};

use crate::{
    desk_error::DeskError,
    model::data_channel::{MouseEventData, MouseEventHandler},
};

pub struct UinputMouseEventHandler {
    pub virtual_device: VirtualDevice,
}

impl UinputMouseEventHandler {
    pub fn new(_width: i32, _height: i32) -> Result<Self, DeskError> {
        // https://www.kernel.org/doc/html/v4.12/input/uinput.html
        let mut keys = AttributeSet::<KeyCode>::new();
        keys.insert(evdev::KeyCode::BTN_LEFT);
        keys.insert(evdev::KeyCode::BTN_RIGHT);
        keys.insert(evdev::KeyCode::BTN_MIDDLE);

        let abs_setup = AbsInfo::new(0, 0, 32767, 0, 0, 0);

        let abs_x = UinputAbsSetup::new(AbsoluteAxisCode::ABS_X, abs_setup.clone());
        let abs_y = UinputAbsSetup::new(AbsoluteAxisCode::ABS_Y, abs_setup.clone());

        let mut rel_axes = AttributeSet::new();
        rel_axes.insert(evdev::RelativeAxisCode::REL_WHEEL);
        rel_axes.insert(evdev::RelativeAxisCode::REL_HWHEEL);
        //let keyset = AttributeSetRef::from(keys);
        let virtual_device = VirtualDevice::builder()?
            .name("lcxl-web-remote-desk-mouse")
            .with_keys(&keys)?
            .with_absolute_axis(&abs_x)?
            .with_absolute_axis(&abs_y)?
            .with_relative_axes(&rel_axes)?
            .build()?;
        Ok(Self { virtual_device })
    }
}

impl MouseEventHandler for UinputMouseEventHandler {
    fn handle_mouse_move(&mut self, event: &MouseEventData) -> Result<(), DeskError> {
        let input_event_x = InputEvent::new(
            EventType::ABSOLUTE.0,
            AbsoluteAxisCode::ABS_X.0,
            (event.x * 32767.0) as i32,
        );
        let input_event_y = InputEvent::new(
            EventType::ABSOLUTE.0,
            AbsoluteAxisCode::ABS_Y.0,
            (event.y * 32767.0) as i32,
        );
        self.virtual_device.emit(&[input_event_x, input_event_y])?;
        Ok(())
    }

    fn handle_mouse_down(&mut self, event: &MouseEventData) -> Result<(), DeskError> {
        let code = match event.button {
            0 => evdev::KeyCode::BTN_LEFT,
            1 => evdev::KeyCode::BTN_RIGHT,
            2 => evdev::KeyCode::BTN_MIDDLE,
            _ => {
                log::warn!("Unsupported mouse button: {}", event.button);
                return Ok(());
            }
        };
        let down_event = *KeyEvent::new(code, 1);
        self.virtual_device.emit(&[down_event])?;
        Ok(())
    }

    fn handle_mouse_up(&mut self, event: &MouseEventData) -> Result<(), DeskError> {
        let code = match event.button {
            0 => evdev::KeyCode::BTN_LEFT,
            1 => evdev::KeyCode::BTN_RIGHT,
            2 => evdev::KeyCode::BTN_MIDDLE,
            _ => {
                log::warn!("Unsupported mouse button: {}", event.button);
                return Ok(());
            }
        };
        let up_event = *KeyEvent::new(code, 0);
        self.virtual_device.emit(&[up_event])?;
        Ok(())
    }

    fn handle_mouse_wheel(&mut self, event: &MouseEventData) -> Result<(), DeskError> {
        todo!()
    }
}
