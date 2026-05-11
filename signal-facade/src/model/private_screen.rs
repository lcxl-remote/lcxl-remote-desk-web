use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Data model for EnablePrivateScreen signaling
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct EnablePrivateScreenData {
    pub enable: bool,
}

/// Data model for PrivateScreenStateChanged signaling
#[derive(
    Debug, Clone, Serialize, Deserialize, ToSchema, wincode::SchemaWrite, wincode::SchemaRead,
)]
pub struct PrivateScreenStateChangedData {
    pub visible: bool,
    pub is_supported: bool,
    pub error_msg: Option<String>,
}

#[cfg(test)]
mod wincode_tests {
    use super::*;
    use wincode::config::{Configuration, PREALLOCATION_SIZE_LIMIT_DISABLED};

    fn unbounded_config() -> Configuration<true, PREALLOCATION_SIZE_LIMIT_DISABLED> {
        Configuration::new()
    }

    #[test]
    fn private_screen_state_changed_data_round_trips_wincode() {
        let config = unbounded_config();
        // Exercise both bool combinations + Some / None on
        // `error_msg` so a field reorder or dropped Option tag
        // shows up.
        let cases = [
            PrivateScreenStateChangedData {
                visible: true,
                is_supported: true,
                error_msg: None,
            },
            PrivateScreenStateChangedData {
                visible: false,
                is_supported: false,
                error_msg: Some("hub denied".to_string()),
            },
        ];
        for original in cases {
            let bytes = wincode::config::serialize(&original, config).expect("encode");
            let back: PrivateScreenStateChangedData =
                wincode::config::deserialize(&bytes, config).expect("decode");
            assert_eq!(back.visible, original.visible);
            assert_eq!(back.is_supported, original.is_supported);
            assert_eq!(back.error_msg, original.error_msg);
        }
    }
}
