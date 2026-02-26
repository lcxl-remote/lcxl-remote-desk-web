use crate::{
    error::DeskError,
    model::data_channel::{KeyboardEventData, KeyboardEventHandler},
};
use core_graphics::event::{CGEvent, CGEventTapLocation, CGKeyCode, KeyCode};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use desk_utils::error::DeskErrorCode;

pub struct MacKeyboardEventHandler {}

impl MacKeyboardEventHandler {
    pub fn new() -> Self {
        Self {}
    }

    fn create_source() -> Result<CGEventSource, DeskError> {
        match CGEventSource::new(CGEventSourceStateID::HIDSystemState) {
            Ok(source) => Ok(source),
            Err(_) => DeskError::custom_error(
                DeskErrorCode::SYSTEM_ERROR,
                "Failed to create event source"
            ),
        }
    }
}

impl KeyboardEventHandler for MacKeyboardEventHandler {
    fn handle_key_down(&mut self, event: &KeyboardEventData) -> Result<(), DeskError> {
        if let Some(keycode) = win_vk_to_mac_keycode(event.key_code) {
            let source = Self::create_source()?;
            match CGEvent::new_keyboard_event(source, keycode, true) {
                Ok(cg_event) => {
                    cg_event.post(CGEventTapLocation::HID);
                    Ok(())
                }
                Err(_) => DeskError::custom_error(
                    DeskErrorCode::SYSTEM_ERROR,
                    &format!("Failed to create key down event for key {}", event.key_code),
                ),
            }
        } else {
            log::warn!("Unsupported key code: {}", event.key_code);
            Ok(())
        }
    }

    fn handle_key_up(&mut self, event: &KeyboardEventData) -> Result<(), DeskError> {
        if let Some(keycode) = win_vk_to_mac_keycode(event.key_code) {
            let source = Self::create_source()?;
            match CGEvent::new_keyboard_event(source, keycode, false) {
                Ok(cg_event) => {
                    cg_event.post(CGEventTapLocation::HID);
                    Ok(())
                }
                Err(_) => DeskError::custom_error(
                    DeskErrorCode::SYSTEM_ERROR,
                    &format!("Failed to create key up event for key {}", event.key_code),
                ),
            }
        } else {
            log::warn!("Unsupported key code: {}", event.key_code);
            Ok(())
        }
    }
}

/// Map Windows Virtual Key Codes to macOS CGKeyCodes
///
/// Mappings based on:
/// https://learn.microsoft.com/en-us/windows/win32/inputdev/virtual-key-codes
/// https://github.com/phracker/MacOSX-SDKs/.../Events.h
fn win_vk_to_mac_keycode(vk: u32) -> Option<CGKeyCode> {
    match vk {
        // A-Z
        0x41 => Some(0x00), // A
        0x42 => Some(0x0B), // B
        0x43 => Some(0x08), // C
        0x44 => Some(0x02), // D
        0x45 => Some(0x0E), // E
        0x46 => Some(0x03), // F
        0x47 => Some(0x05), // G
        0x48 => Some(0x04), // H
        0x49 => Some(0x22), // I
        0x4A => Some(0x26), // J
        0x4B => Some(0x28), // K
        0x4C => Some(0x25), // L
        0x4D => Some(0x2E), // M
        0x4E => Some(0x2D), // N
        0x4F => Some(0x1F), // O
        0x50 => Some(0x23), // P
        0x51 => Some(0x0C), // Q
        0x52 => Some(0x0F), // R
        0x53 => Some(0x01), // S
        0x54 => Some(0x11), // T
        0x55 => Some(0x20), // U
        0x56 => Some(0x09), // V
        0x57 => Some(0x0D), // W
        0x58 => Some(0x07), // X
        0x59 => Some(0x10), // Y
        0x5A => Some(0x06), // Z

        // 0-9 (Main Keyboard)
        0x30 => Some(0x1D), // 0
        0x31 => Some(0x12), // 1
        0x32 => Some(0x13), // 2
        0x33 => Some(0x14), // 3
        0x34 => Some(0x15), // 4
        0x35 => Some(0x17), // 5
        0x36 => Some(0x16), // 6
        0x37 => Some(0x1A), // 7
        0x38 => Some(0x1C), // 8
        0x39 => Some(0x19), // 9

        // Function Keys
        0x70 => Some(KeyCode::F1),
        0x71 => Some(KeyCode::F2),
        0x72 => Some(KeyCode::F3),
        0x73 => Some(KeyCode::F4),
        0x74 => Some(KeyCode::F5),
        0x75 => Some(KeyCode::F6),
        0x76 => Some(KeyCode::F7),
        0x77 => Some(KeyCode::F8),
        0x78 => Some(KeyCode::F9),
        0x79 => Some(KeyCode::F10),
        0x7A => Some(KeyCode::F11),
        0x7B => Some(KeyCode::F12),

        // Modifiers
        0x10 => Some(KeyCode::SHIFT),
        0xA0 => Some(KeyCode::SHIFT),       // VK_LSHIFT
        0xA1 => Some(KeyCode::RIGHT_SHIFT), // VK_RSHIFT
        0x11 => Some(KeyCode::CONTROL),
        0xA2 => Some(KeyCode::CONTROL),       // VK_LCONTROL
        0xA3 => Some(KeyCode::RIGHT_CONTROL), // VK_RCONTROL
        0x12 => Some(KeyCode::OPTION),        // VK_MENU (Alt)
        0xA4 => Some(KeyCode::OPTION),        // VK_LMENU
        0xA5 => Some(KeyCode::RIGHT_OPTION),  // VK_RMENU
        0x5B => Some(KeyCode::COMMAND),       // VK_LWIN (Command)
        0x5C => Some(KeyCode::RIGHT_COMMAND), // VK_RWIN

        // Special Keys
        0x0D => Some(KeyCode::RETURN), // Enter
        0x08 => Some(KeyCode::DELETE), // Backspace (mapped to macOS Delete)
        0x09 => Some(KeyCode::TAB),
        0x20 => Some(KeyCode::SPACE),
        0x1B => Some(KeyCode::ESCAPE),
        0x25 => Some(KeyCode::LEFT_ARROW),
        0x26 => Some(KeyCode::UP_ARROW),
        0x27 => Some(KeyCode::RIGHT_ARROW),
        0x28 => Some(KeyCode::DOWN_ARROW),
        0x2E => Some(KeyCode::FORWARD_DELETE), // Delete (mapped to macOS Forward Delete)

        // Punctuation (US Layout)
        0xBD => Some(0x1B), // -
        0xBB => Some(0x18), // =
        0xDB => Some(0x21), // [
        0xDD => Some(0x1E), // ]
        0xDC => Some(0x2A), // \
        0xBA => Some(0x29), // ;
        0xDE => Some(0x27), // '
        0xBC => Some(0x2B), // ,
        0xBE => Some(0x2F), // .
        0xBF => Some(0x2C), // /
        0xC0 => Some(0x32), // `

        _ => None,
    }
}
