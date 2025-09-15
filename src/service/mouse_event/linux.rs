use evdev::{uinput::VirtualDevice, AbsInfo, AbsoluteAxisCode, AttributeSet, AttributeSetRef, InputEvent, KeyCode, UinputAbsSetup};

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
    fn handle_mouse_move(&self, event: &MouseEventData) -> Result<(), DeskError> {
        let input_event = InputEvent::new(EV_REL)
         self.virtual_device.emit()
        todo!()
    }

    fn handle_mouse_down(&self, event: &MouseEventData) -> Result<(), DeskError> {
        self.virtual_device.emit(event)?;
        Ok(())
    }

    fn handle_mouse_up(&self, event: &MouseEventData) -> Result<(), DeskError> {
        todo!()
    }

    fn handle_mouse_wheel(&self, event: &MouseEventData) -> Result<(), DeskError> {
        todo!()
    }
}
