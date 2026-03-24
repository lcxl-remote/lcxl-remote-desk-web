use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Clone, Debug, Default, Deserialize, Serialize, ToSchema, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum TraversalMode {
    /// desk server will use turn server to relay
    #[default]
    Turn,
    /// desk server will use stun server to relay
    Stun,
    /// desk server will not use turn/stun server to relay
    None,
}

/// Turn client settings
/// Desk server as a turn client to connect to the turn server
#[derive(Clone, Debug, Default, Deserialize, Serialize, ToSchema)]
pub struct TurnClientSettings {
    /// Traversal mode
    pub traversal_mode: TraversalMode,
}
