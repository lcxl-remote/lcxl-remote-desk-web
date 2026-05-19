use evdev::{
    AbsInfo, AbsoluteAxisCode, AttributeSet, EventType, InputEvent, KeyCode, KeyEvent,
    UinputAbsSetup, uinput::VirtualDevice,
};

use crate::{
    error::InputError,
    model::data_channel::{MouseEventData, MouseEventHandler},
};

pub struct UinputMouseEventHandler {
    pub virtual_device: VirtualDevice,
    pub wheel_acc_x: f64,
    pub wheel_acc_y: f64,
}

impl UinputMouseEventHandler {
    /// `left` / `top` / `width` / `height` are accepted for signature
    /// uniformity with the Windows and macOS backends, but uinput's
    /// absolute axis range is a fixed `0..32767` that the X / Wayland
    /// compositor maps to its own screen / output. Applying a
    /// virtual-desktop offset here would push the cursor outside the
    /// reachable range, so the parameters are intentionally ignored.
    /// Multi-monitor cursor targeting on uinput would require either a
    /// separate virtual device per output or relative-mode emulation,
    /// neither of which is in scope today.
    pub fn new(
        _left: i32,
        _top: i32,
        _width: i32,
        _height: i32,
    ) -> Result<Self, InputError> {
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
            .input_id(evdev::InputId::new(
                evdev::BusType::BUS_USB,
                0x1234,
                0x5678,
                0x111,
            ))
            .with_keys(&keys)?
            .with_absolute_axis(&abs_x)?
            .with_absolute_axis(&abs_y)?
            .with_relative_axes(&rel_axes)?
            .build()?;
        Ok(Self {
            virtual_device,
            wheel_acc_x: 0.0,
            wheel_acc_y: 0.0,
        })
    }
}

impl MouseEventHandler for UinputMouseEventHandler {
    fn handle_mouse_move(&mut self, event: &MouseEventData) -> Result<(), InputError> {
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
        let syn_event = InputEvent::new(EventType::SYNCHRONIZATION.0, 0, 0);
        let result = self
            .virtual_device
            .emit(&[input_event_x, input_event_y, syn_event]);
        if let Err(e) = result {
            log::error!("Failed to emit mouse move event: {}", e);
            return Err(InputError::from(e));
        } else {
            log::trace!(
                "Mouse move event emitted successfully, x: {}, y: {}",
                event.x,
                event.y
            );
        }
        Ok(())
    }

    fn handle_mouse_down(&mut self, event: &MouseEventData) -> Result<(), InputError> {
        let code = match event.button {
            0 => evdev::KeyCode::BTN_LEFT,
            1 => evdev::KeyCode::BTN_MIDDLE,
            2 => evdev::KeyCode::BTN_RIGHT,
            _ => {
                log::warn!("Unsupported mouse button: {}", event.button);
                return Ok(());
            }
        };
        let down_event = *KeyEvent::new(code, 1);
        let syn_event = InputEvent::new(EventType::SYNCHRONIZATION.0, 0, 0);
        let result = self.virtual_device.emit(&[down_event, syn_event]);
        if let Err(e) = result {
            log::error!("Failed to emit mouse down event: {}", e);
            return Err(InputError::from(e));
        } else {
            log::info!(
                "Mouse down event emitted successfully, button: {}",
                event.button
            );
        }
        Ok(())
    }

    fn handle_mouse_up(&mut self, event: &MouseEventData) -> Result<(), InputError> {
        let code = match event.button {
            0 => evdev::KeyCode::BTN_LEFT,
            1 => evdev::KeyCode::BTN_MIDDLE,
            2 => evdev::KeyCode::BTN_RIGHT,
            _ => {
                log::warn!("Unsupported mouse button: {}", event.button);
                return Ok(());
            }
        };
        let up_event = *KeyEvent::new(code, 0);
        let syn_event = InputEvent::new(EventType::SYNCHRONIZATION.0, 0, 0);
        let result = self.virtual_device.emit(&[up_event, syn_event]);
        if let Err(e) = result {
            log::error!("Failed to emit mouse up event: {}", e);
            return Err(InputError::from(e));
        } else {
            log::info!(
                "Mouse up event emitted successfully, button: {}",
                event.button
            );
        }
        Ok(())
    }

    fn handle_mouse_wheel(&mut self, event: &MouseEventData) -> Result<(), InputError> {
        self.wheel_acc_y += event.delta_y;
        self.wheel_acc_x += event.delta_x;

        let step = 100.0;
        let mut ticks_y = 0;
        while self.wheel_acc_y >= step {
            ticks_y += 1;
            self.wheel_acc_y -= step;
        }
        while self.wheel_acc_y <= -step {
            ticks_y -= 1;
            self.wheel_acc_y += step;
        }

        let mut ticks_x = 0;
        while self.wheel_acc_x >= step {
            ticks_x += 1;
            self.wheel_acc_x -= step;
        }
        while self.wheel_acc_x <= -step {
            ticks_x -= 1;
            self.wheel_acc_x += step;
        }

        if ticks_x == 0 && ticks_y == 0 {
            return Ok(());
        }

        let mut events = Vec::new();
        if ticks_y != 0 {
            let wheel_event_y = evdev::InputEvent::new(
                evdev::EventType::RELATIVE.0,
                evdev::RelativeAxisCode::REL_WHEEL.0,
                -ticks_y,
            );
            events.push(wheel_event_y);
        }
        if ticks_x != 0 {
            let wheel_event_x = evdev::InputEvent::new(
                evdev::EventType::RELATIVE.0,
                evdev::RelativeAxisCode::REL_HWHEEL.0,
                ticks_x,
            );
            events.push(wheel_event_x);
        }

        events.push(evdev::InputEvent::new(
            evdev::EventType::SYNCHRONIZATION.0,
            evdev::SynchronizationCode::SYN_REPORT.0,
            0,
        ));

        self.virtual_device.emit(&events)?;
        Ok(())
    }
}
