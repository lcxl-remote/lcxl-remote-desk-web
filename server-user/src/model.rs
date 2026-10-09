use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Serialize, Deserialize, Debug, ToSchema)]
pub struct LabelKey {
    pub label: Option<String>,
    pub key: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, ToSchema)]
pub struct Geographic {
    pub province: Option<LabelKey>,
    pub city: Option<LabelKey>,
}

use desk_signal_facade::model::signal::SignalingUser;

pub const USER_ADMIN: &str = "admin";
/// Trait for base user information.
pub trait BaseUser: SignalingUser {
    fn get_name(&self) -> &str;
}

#[derive(Serialize, Deserialize, Debug, ToSchema)]
pub struct CurrentUser {
    pub name: String,
    pub avatar: Option<String>,
    pub userid: Option<String>,
    pub email: Option<String>,
    pub signature: Option<String>,
    pub title: Option<String>,
    pub group: Option<String>,
    pub tags: Option<Vec<LabelKey>>,
    #[serde(rename(serialize = "notifyCount"))]
    #[schema(rename = "notifyCount")]
    pub notify_count: Option<u32>,
    #[serde(rename(serialize = "unreadCount"))]
    #[schema(rename = "unreadCount")]
    pub unread_count: Option<u32>,
    pub country: Option<String>,
    pub access: Option<String>,
    // Session storage serializes and deserializes this same public identity.
    // Losing the target fence would broaden a scoped user's authority.
    #[serde(rename = "targetConnectionId")]
    #[schema(rename = "targetConnectionId")]
    pub target_connection_id: Option<String>,
    pub geographic: Option<Geographic>,
    pub address: Option<String>,
    pub phone: Option<String>,
}

impl CurrentUser {
    pub fn new_admin(name: &str) -> Self {
        CurrentUser {
            name: name.to_string(),
            avatar: None,
            userid: None,
            email: None,
            signature: None,
            title: None,
            group: None,
            tags: None,
            notify_count: None,
            unread_count: None,
            country: None,
            access: Some(USER_ADMIN.to_string()),
            target_connection_id: None,
            geographic: None,
            address: None,
            phone: None,
        }
    }
}

impl BaseUser for CurrentUser {
    fn get_name(&self) -> &str {
        &self.name
    }
}

impl SignalingUser for CurrentUser {
    fn get_access(&self) -> Option<&str> {
        self.access.as_deref()
    }

    fn get_target_connection_id(&self) -> Option<&str> {
        self.target_connection_id.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_round_trip_preserves_the_target_authority_fence() {
        let mut scoped = CurrentUser::new_admin("scoped");
        scoped.target_connection_id = Some("one-device".into());
        let serialized = serde_json::to_value(&scoped).unwrap();
        assert_eq!(serialized["targetConnectionId"], "one-device");
        assert!(serialized.get("target_connection_id").is_none());
        let restored: CurrentUser = serde_json::from_value(serialized).unwrap();
        assert_eq!(restored.get_target_connection_id(), Some("one-device"));
        assert_eq!(restored.get_access(), Some(USER_ADMIN));
        let owner: CurrentUser =
            serde_json::from_value(serde_json::to_value(CurrentUser::new_admin("owner")).unwrap())
                .unwrap();
        assert_eq!(owner.get_target_connection_id(), None);
    }
}

#[derive(Serialize, Debug, ToSchema)]
pub enum NoticeIconItemType {
    #[serde(rename(serialize = "notification"))]
    #[schema(rename = "notification")]
    Notification,
    #[serde(rename(serialize = "message"))]
    #[schema(rename = "message")]
    Message,
    #[serde(rename(serialize = "event"))]
    #[schema(rename = "event")]
    Event,
}

#[derive(Serialize, Debug, ToSchema)]
pub struct NoticeIconItem {
    pub id: Option<String>,
    pub extra: Option<String>,
    pub key: Option<String>,
    pub read: Option<bool>,
    pub avatar: Option<String>,
    pub title: Option<String>,
    pub status: Option<String>,
    pub datetime: Option<String>,
    pub description: Option<String>,
    #[serde(rename(serialize = "type"))]
    #[schema(rename = "type")]
    pub notice_type: Option<NoticeIconItemType>,
}

#[derive(Serialize, Debug, ToSchema)]
pub struct NoticeIconList {
    pub data: Option<Vec<NoticeIconItem>>,
    pub total: u32,
    pub success: bool,
}
