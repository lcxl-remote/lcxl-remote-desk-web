//! PID-targeted application input, independent of foreground input fallback.
use crate::computer_use::ObjectRef;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use wincode::{SchemaRead, SchemaWrite};

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    SchemaWrite,
    SchemaRead,
    ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ApplicationActionKind {
    Invoke,
    Select,
    Focus,
    Toggle,
    SetValue,
    Click,
    DoubleClick,
    Scroll,
    TypeText,
    KeyPress,
}
impl ApplicationActionKind {
    pub fn is_background(self) -> bool {
        matches!(
            self,
            Self::Click | Self::DoubleClick | Self::Scroll | Self::TypeText | Self::KeyPress
        )
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub enum InputModifier {
    Command,
    Control,
    Option,
    Shift,
}

/// Pixel coordinates in the original window screenshot, with a top-left origin.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(deny_unknown_fields)]
pub struct WindowInputPosition {
    pub x: u32,
    pub y: u32,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum BackgroundInputAction {
    Click {
        position: Option<WindowInputPosition>,
        element: Option<ObjectRef>,
    },
    DoubleClick {
        position: Option<WindowInputPosition>,
        element: Option<ObjectRef>,
    },
    Scroll {
        position: Option<WindowInputPosition>,
        element: Option<ObjectRef>,
        #[serde(rename = "horizontal_pixels")]
        horizontal: i32,
        #[serde(rename = "vertical_pixels")]
        vertical: i32,
    },
    TypeText {
        text: String,
    },
    KeyPress {
        key: String,
        #[serde(default)]
        modifiers: Vec<InputModifier>,
    },
}
impl BackgroundInputAction {
    pub fn kind(&self) -> ApplicationActionKind {
        match self {
            Self::Click { .. } => ApplicationActionKind::Click,
            Self::DoubleClick { .. } => ApplicationActionKind::DoubleClick,
            Self::Scroll { .. } => ApplicationActionKind::Scroll,
            Self::TypeText { .. } => ApplicationActionKind::TypeText,
            Self::KeyPress { .. } => ApplicationActionKind::KeyPress,
        }
    }
    pub fn locator(&self) -> Option<(&Option<WindowInputPosition>, &Option<ObjectRef>)> {
        match self {
            Self::Click { position, element }
            | Self::DoubleClick { position, element }
            | Self::Scroll {
                position, element, ..
            } => Some((position, element)),
            _ => None,
        }
    }
    pub fn validate(&self) -> Result<(), &'static str> {
        if let Some((position, element)) = self.locator() {
            if position.is_some() == element.is_some() {
                return Err("Provide exactly one position {x,y} or element_id for mouse input");
            }
            if element.as_ref().is_some_and(|e| {
                e.object_kind != crate::computer_use::ObjectKind::UiElement
                    || e.token.is_empty()
                    || e.snapshot_id.is_empty()
            }) {
                return Err("Mouse element_id must identify an observed UI element");
            }
        }
        match self {
            Self::KeyPress { key, modifiers } => {
                if key_code(key).is_none() {
                    return Err(
                        "Unknown key. Use a letter/digit, Enter, Tab, Escape, Backspace, Delete, Space, ArrowLeft/Right/Up/Down, Home, End, PageUp/Down, or F1–F12; modifiers: Command, Control, Option, Shift",
                    );
                }
                if modifiers.len() > 4
                    || modifiers
                        .iter()
                        .enumerate()
                        .any(|(i, m)| modifiers[..i].contains(m))
                {
                    return Err("Modifiers must be unique Command/Control/Option/Shift values");
                }
            }
            Self::TypeText { text } if text.is_empty() || text.len() > 16 * 1024 => {
                return Err("type_text requires 1–16384 UTF-8 bytes");
            }
            Self::Scroll {
                horizontal,
                vertical,
                ..
            } if horizontal.unsigned_abs() > 10000 || vertical.unsigned_abs() > 10000 => {
                return Err("Scroll deltas must be between -10000 and 10000 pixels");
            }
            _ => {}
        }
        Ok(())
    }
}
/// Hardware key locations; arbitrary text is carried by Unicode events instead.
pub fn key_code(key: &str) -> Option<u16> {
    Some(match key {
        "Enter" => 36,
        "Tab" => 48,
        "Escape" => 53,
        "Backspace" => 51,
        "Delete" => 117,
        "Space" => 49,
        "ArrowLeft" => 123,
        "ArrowRight" => 124,
        "ArrowDown" => 125,
        "ArrowUp" => 126,
        "Home" => 115,
        "End" => 119,
        "PageUp" => 116,
        "PageDown" => 121,
        "F1" => 122,
        "F2" => 120,
        "F3" => 99,
        "F4" => 118,
        "F5" => 96,
        "F6" => 97,
        "F7" => 98,
        "F8" => 100,
        "F9" => 101,
        "F10" => 109,
        "F11" => 103,
        "F12" => 111,
        _ => match key.to_ascii_lowercase().as_str() {
            "a" => 0,
            "s" => 1,
            "d" => 2,
            "f" => 3,
            "h" => 4,
            "g" => 5,
            "z" => 6,
            "x" => 7,
            "c" => 8,
            "v" => 9,
            "b" => 11,
            "q" => 12,
            "w" => 13,
            "e" => 14,
            "r" => 15,
            "y" => 16,
            "t" => 17,
            "1" => 18,
            "2" => 19,
            "3" => 20,
            "4" => 21,
            "6" => 22,
            "5" => 23,
            "=" => 24,
            "9" => 25,
            "7" => 26,
            "-" => 27,
            "8" => 28,
            "0" => 29,
            "]" => 30,
            "o" => 31,
            "u" => 32,
            "[" => 33,
            "i" => 34,
            "p" => 35,
            "l" => 37,
            "j" => 38,
            "'" => 39,
            "k" => 40,
            ";" => 41,
            "\\" => 42,
            "," => 43,
            "/" => 44,
            "n" => 45,
            "m" => 46,
            "." => 47,
            "`" => 50,
            _ => return None,
        },
    })
}

/// Original captured window size in thousandths of a Quartz point; independent of image scaling.
#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct WindowInputGeometry {
    pub width_pixels: u32,
    pub height_pixels: u32,
    pub width_millipoints: u64,
    pub height_millipoints: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scroll_fields_require_explicit_pixel_units() {
        let action: BackgroundInputAction = serde_json::from_value(serde_json::json!({
            "kind":"scroll", "position":{"x":550,"y":300},
            "horizontal_pixels":0,"vertical_pixels":-300
        }))
        .unwrap();
        assert!(action.validate().is_ok());
        let value = serde_json::to_value(action).unwrap();
        assert_eq!(value["vertical_pixels"], -300);
        assert!(value.get("vertical").is_none());
        assert!(
            serde_json::from_value::<BackgroundInputAction>(serde_json::json!({
                "kind":"scroll", "position":{"x":550,"y":300},"horizontal":0,"vertical":-6
            }))
            .is_err()
        );
    }
    #[test]
    fn pixel_positions_are_not_limited_to_one_thousand() {
        let action: BackgroundInputAction = serde_json::from_value(serde_json::json!({
            "kind":"click","position":{"x":1920,"y":1080}
        }))
        .unwrap();
        assert!(action.validate().is_ok());
    }
    #[test]
    fn closed_actions_reject_ambiguous_locators_and_bad_keys() {
        for value in [
            serde_json::json!({"kind":"click"}),
            serde_json::json!({"kind":"key_press","key":"launch_shell"}),
            serde_json::json!({"kind":"key_press","key":"a","modifiers":["Command","Command"]}),
            serde_json::json!({"kind":"type_text","text":""}),
        ] {
            assert!(
                serde_json::from_value::<BackgroundInputAction>(value)
                    .unwrap()
                    .validate()
                    .is_err()
            );
        }
        assert!(
            serde_json::from_value::<BackgroundInputAction>(
                serde_json::json!({"kind":"key_press","key":"a","pid":1})
            )
            .is_err()
        );
        assert_eq!(key_code("ArrowRight"), Some(124));
        assert_eq!(key_code("A"), key_code("a"));
        assert!(key_code("你好").is_none());
    }
}
