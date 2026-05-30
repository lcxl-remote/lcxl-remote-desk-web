use evdev::{KeyCode, KeyEvent, uinput::VirtualDevice};

use crate::{
    error::InputError,
    linux_display::UINPUT_KEYBOARD_DEVICE_NAME,
    model::data_channel::{KeyboardEventData, KeyboardEventHandler},
};

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
        if let Some(evdev_code) = web_code_to_evdev(&event.code) {
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
        if let Some(evdev_code) = web_code_to_evdev(&event.code) {
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

fn web_code_to_evdev(code: &str) -> Option<u16> {
    match code {
        "Escape" => Some(1),
        "Digit1" => Some(2),
        "Digit2" => Some(3),
        "Digit3" => Some(4),
        "Digit4" => Some(5),
        "Digit5" => Some(6),
        "Digit6" => Some(7),
        "Digit7" => Some(8),
        "Digit8" => Some(9),
        "Digit9" => Some(10),
        "Digit0" => Some(11),
        "Minus" => Some(12),
        "Equal" => Some(13),
        "Backspace" => Some(14),
        "Tab" => Some(15),
        "KeyQ" => Some(16),
        "KeyW" => Some(17),
        "KeyE" => Some(18),
        "KeyR" => Some(19),
        "KeyT" => Some(20),
        "KeyY" => Some(21),
        "KeyU" => Some(22),
        "KeyI" => Some(23),
        "KeyO" => Some(24),
        "KeyP" => Some(25),
        "BracketLeft" => Some(26),
        "BracketRight" => Some(27),
        "Enter" => Some(28),
        "ControlLeft" => Some(29),
        "KeyA" => Some(30),
        "KeyS" => Some(31),
        "KeyD" => Some(32),
        "KeyF" => Some(33),
        "KeyG" => Some(34),
        "KeyH" => Some(35),
        "KeyJ" => Some(36),
        "KeyK" => Some(37),
        "KeyL" => Some(38),
        "Semicolon" => Some(39),
        "Quote" => Some(40),
        "Backquote" => Some(41),
        "ShiftLeft" => Some(42),
        "Backslash" => Some(43),
        "KeyZ" => Some(44),
        "KeyX" => Some(45),
        "KeyC" => Some(46),
        "KeyV" => Some(47),
        "KeyB" => Some(48),
        "KeyN" => Some(49),
        "KeyM" => Some(50),
        "Comma" => Some(51),
        "Period" => Some(52),
        "Slash" => Some(53),
        "ShiftRight" => Some(54),
        "NumpadMultiply" => Some(55),
        "AltLeft" => Some(56),
        "Space" => Some(57),
        "CapsLock" => Some(58),
        "F1" => Some(59),
        "F2" => Some(60),
        "F3" => Some(61),
        "F4" => Some(62),
        "F5" => Some(63),
        "F6" => Some(64),
        "F7" => Some(65),
        "F8" => Some(66),
        "F9" => Some(67),
        "F10" => Some(68),
        "NumLock" => Some(69),
        "ScrollLock" => Some(70),
        "Numpad7" => Some(71),
        "Numpad8" => Some(72),
        "Numpad9" => Some(73),
        "NumpadSubtract" => Some(74),
        "Numpad4" => Some(75),
        "Numpad5" => Some(76),
        "Numpad6" => Some(77),
        "NumpadAdd" => Some(78),
        "Numpad1" => Some(79),
        "Numpad2" => Some(80),
        "Numpad3" => Some(81),
        "Numpad0" => Some(82),
        "NumpadDecimal" => Some(83),
        "F11" => Some(87),
        "F12" => Some(88),
        "NumpadEnter" => Some(96),
        "ControlRight" => Some(97),
        "NumpadDivide" => Some(98),
        "PrintScreen" => Some(99),
        "AltRight" => Some(100),
        "Home" => Some(102),
        "ArrowUp" => Some(103),
        "PageUp" => Some(104),
        "ArrowLeft" => Some(105),
        "ArrowRight" => Some(106),
        "End" => Some(107),
        "ArrowDown" => Some(108),
        "PageDown" => Some(109),
        "Insert" => Some(110),
        "Delete" => Some(111),
        "MetaLeft" | "OSLeft" => Some(125),
        "MetaRight" | "OSRight" => Some(126),
        "ContextMenu" => Some(127),
        _ => None,
    }
}
