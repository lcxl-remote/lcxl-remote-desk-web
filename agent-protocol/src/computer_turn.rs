//! A device may release a local control period after checking its frozen turn.
//! These messages never authorize, renew, or replay an action.
use crate::computer_use::{ComputerActionTurnScope, ComputerUseValidationError};
use serde::{Deserialize, Serialize};
use wincode::{SchemaRead, SchemaWrite};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, SchemaRead, SchemaWrite)]
#[serde(deny_unknown_fields)]
pub struct ComputerActionTurnQuery {
    pub actor_id: String,
    pub scope: ComputerActionTurnScope,
}

impl ComputerActionTurnQuery {
    pub fn validate(&self) -> Result<(), ComputerUseValidationError> {
        self.scope.validate()?;
        if self.actor_id.trim().is_empty()
            || self.actor_id.len() > 256
            || self.actor_id.chars().any(char::is_control)
        {
            return Err(ComputerUseValidationError::InvalidContextReference(
                "invalid turn query actor".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, SchemaRead, SchemaWrite)]
#[serde(rename_all = "snake_case")]
pub enum ComputerActionTurnState {
    Current,
    Revoked,
    Unavailable,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, SchemaRead, SchemaWrite)]
#[serde(deny_unknown_fields)]
pub struct ComputerActionTurnStatus {
    pub query: ComputerActionTurnQuery,
    pub state: ComputerActionTurnState,
}
