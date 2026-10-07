//! Bounded automatic status delivery is independent of a model's explicit wait.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotificationEvent {
    pub event_id: String,
    pub task_id: String,
    pub state_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParentNotification {
    pub message_id: String,
    pub group_id: String,
    pub source_epoch: u64,
    pub parent_input_revision: u64,
    pub parent_control_revision: u64,
    pub events: Vec<NotificationEvent>,
    pub retry_after_ms: Option<i64>,
    /// Staged only after the actual model request contained this message.
    /// The store independently checks provider evidence before marking delivery.
    pub accepted_response_message_id: Option<String>,
}

impl ParentNotification {
    pub fn validate(&self) -> Result<(), &'static str> {
        let unique: std::collections::BTreeSet<_> =
            self.events.iter().map(|event| &event.event_id).collect();
        let tasks: std::collections::BTreeSet<_> =
            self.events.iter().map(|event| &event.task_id).collect();
        if !super::valid_id(&self.message_id)
            || !super::valid_id(&self.group_id)
            || self.source_epoch == 0
            || self.parent_input_revision == 0
            || self.parent_control_revision == 0
            || self.events.is_empty()
            || self.events.len() > super::MAX_SUBAGENT_NOTIFICATION_EVENTS
            || unique.len() != self.events.len()
            || tasks.len() != self.events.len()
            || self.events.iter().any(|event| {
                !super::valid_id(&event.event_id)
                    || !super::valid_id(&event.task_id)
                    || event.state_revision == 0
            })
            || self.retry_after_ms.is_some_and(|due| due < 0)
            || self
                .accepted_response_message_id
                .as_deref()
                .is_some_and(|id| !super::valid_id(id))
        {
            return Err("invalid parent notification");
        }
        Ok(())
    }
}
