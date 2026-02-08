use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Operation system enum
#[derive(Debug, Clone, Copy, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub enum OperationSystemEnum {
    /// Windows
    Windows,
    /// Linux
    Linux,
    /// Mac
    Mac,
    /// Android
    Android,
    /// iOS
    Ios,
    /// Web
    Web,
    /// Other
    #[serde(other)]
    Other,
}

impl Default for OperationSystemEnum {
    fn default() -> Self {
        // use cfg! macro does not cause performance loss in release mode
        if cfg!(windows) {
            OperationSystemEnum::Windows
        } else if cfg!(target_os = "linux") {
            OperationSystemEnum::Linux
        } else if cfg!(target_os = "macos") {
            OperationSystemEnum::Mac
        } else if cfg!(target_os = "android") {
            OperationSystemEnum::Android
        } else if cfg!(target_os = "ios") {
            OperationSystemEnum::Ios
        } else if cfg!(target_family = "wasm") {
            OperationSystemEnum::Web
        } else {
            OperationSystemEnum::Other
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deserialize_known_variants() {
        let json = r#""Windows""#;
        let os: OperationSystemEnum = serde_json::from_str(json).unwrap();
        assert_eq!(os, OperationSystemEnum::Windows);

        let json = r#""Linux""#;
        let os: OperationSystemEnum = serde_json::from_str(json).unwrap();
        assert_eq!(os, OperationSystemEnum::Linux);
    }

    #[test]
    fn test_deserialize_unknown_variant() {
        let json = r#""Solaris""#;
        let os: OperationSystemEnum = serde_json::from_str(json).unwrap();
        assert_eq!(os, OperationSystemEnum::Other);

        let json = r#""UnknownOS""#;
        let os: OperationSystemEnum = serde_json::from_str(json).unwrap();
        assert_eq!(os, OperationSystemEnum::Other);
    }

    #[test]
    fn test_default() {
        let default_os = OperationSystemEnum::default();
        if cfg!(windows) {
            assert_eq!(default_os, OperationSystemEnum::Windows);
        } else if cfg!(target_os = "linux") {
            assert_eq!(default_os, OperationSystemEnum::Linux);
        } else if cfg!(target_os = "macos") {
            assert_eq!(default_os, OperationSystemEnum::Mac);
        } else {
            // For other environments or catch-all
            // assert_eq!(default_os, OperationSystemEnum::Other);
        }
    }
}
