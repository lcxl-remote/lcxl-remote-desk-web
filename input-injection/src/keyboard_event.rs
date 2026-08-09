#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "linux")]
pub mod wayland_portal;
#[cfg(target_os = "windows")]
pub mod windows;

#[cfg(target_os = "macos")]
pub mod mac;

pub mod keyboard_event_factory;

#[cfg(target_os = "linux")]
pub(super) fn keyboard_event_to_evdev(
    event: &crate::model::data_channel::KeyboardEventData,
) -> Option<u16> {
    dom_code_to_evdev(&event.code).or_else(|| windows_vk_to_evdev(event.key_code))
}

#[cfg(target_os = "linux")]
fn dom_code_to_evdev(code: &str) -> Option<u16> {
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

#[cfg(target_os = "linux")]
fn windows_vk_to_evdev(key_code: u32) -> Option<u16> {
    match key_code {
        0x08 => Some(14),
        0x09 => Some(15),
        0x0D => Some(28),
        0x10 | 0xA0 => Some(42),
        0x11 | 0xA2 => Some(29),
        0x12 | 0xA4 => Some(56),
        0x14 => Some(58),
        0x1B => Some(1),
        0x20 => Some(57),
        0x21 => Some(104),
        0x22 => Some(109),
        0x23 => Some(107),
        0x24 => Some(102),
        0x25 => Some(105),
        0x26 => Some(103),
        0x27 => Some(106),
        0x28 => Some(108),
        0x2C => Some(99),
        0x2D => Some(110),
        0x2E => Some(111),
        0x30 => Some(11),
        0x31..=0x39 => Some((key_code - 0x31 + 2) as u16),
        0x41 => Some(30),
        0x42 => Some(48),
        0x43 => Some(46),
        0x44 => Some(32),
        0x45 => Some(18),
        0x46 => Some(33),
        0x47 => Some(34),
        0x48 => Some(35),
        0x49 => Some(23),
        0x4A => Some(36),
        0x4B => Some(37),
        0x4C => Some(38),
        0x4D => Some(50),
        0x4E => Some(49),
        0x4F => Some(24),
        0x50 => Some(25),
        0x51 => Some(16),
        0x52 => Some(19),
        0x53 => Some(31),
        0x54 => Some(20),
        0x55 => Some(22),
        0x56 => Some(47),
        0x57 => Some(17),
        0x58 => Some(45),
        0x59 => Some(21),
        0x5A => Some(44),
        0x5B => Some(125),
        0x5C => Some(126),
        0x5D => Some(127),
        0x60 => Some(82),
        0x61 => Some(79),
        0x62 => Some(80),
        0x63 => Some(81),
        0x64 => Some(75),
        0x65 => Some(76),
        0x66 => Some(77),
        0x67 => Some(71),
        0x68 => Some(72),
        0x69 => Some(73),
        0x6A => Some(55),
        0x6B => Some(78),
        0x6D => Some(74),
        0x6E => Some(83),
        0x6F => Some(98),
        0x70..=0x79 => Some((key_code - 0x70 + 59) as u16),
        0x7A => Some(87),
        0x7B => Some(88),
        0x90 => Some(69),
        0x91 => Some(70),
        0xA1 => Some(54),
        0xA3 => Some(97),
        0xA5 => Some(100),
        0xBA => Some(39),
        0xBB => Some(13),
        0xBC => Some(51),
        0xBD => Some(12),
        0xBE => Some(52),
        0xBF => Some(53),
        0xC0 => Some(41),
        0xDB => Some(26),
        0xDC => Some(43),
        0xDD => Some(27),
        0xDE => Some(40),
        _ => None,
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use crate::model::data_channel::KeyboardEventData;

    use super::{dom_code_to_evdev, keyboard_event_to_evdev, windows_vk_to_evdev};

    #[test]
    fn maps_dom_codes_to_linux_evdev_codes() {
        assert_eq!(dom_code_to_evdev("KeyA"), Some(30));
        assert_eq!(dom_code_to_evdev("KeyB"), Some(48));
        assert_eq!(dom_code_to_evdev("F7"), Some(65));
        assert_eq!(dom_code_to_evdev("ControlLeft"), Some(29));
        assert_eq!(dom_code_to_evdev("ControlRight"), Some(97));
        assert_eq!(dom_code_to_evdev("Unsupported"), None);
    }

    #[test]
    fn maps_windows_virtual_keys_for_controllers_without_dom_codes() {
        assert_eq!(windows_vk_to_evdev(0x41), Some(30));
        assert_eq!(windows_vk_to_evdev(0x42), Some(48));
        assert_eq!(windows_vk_to_evdev(0x70), Some(59));
        assert_eq!(windows_vk_to_evdev(0x7B), Some(88));
        assert_eq!(windows_vk_to_evdev(0xA3), Some(97));
        assert_eq!(windows_vk_to_evdev(0xBA), Some(39));
        assert_eq!(windows_vk_to_evdev(0xDE), Some(40));
        assert_eq!(windows_vk_to_evdev(0), None);
    }

    #[test]
    fn dom_code_takes_precedence_over_legacy_virtual_key() {
        let event = KeyboardEventData {
            code: "KeyA".to_owned(),
            key_code: 0x42,
            ..Default::default()
        };
        assert_eq!(keyboard_event_to_evdev(&event), Some(30));

        let legacy_event = KeyboardEventData {
            key_code: 0x42,
            ..Default::default()
        };
        assert_eq!(keyboard_event_to_evdev(&legacy_event), Some(48));
    }
}
