use serde::Serialize;
use utoipa::ToSchema;

#[derive(Serialize, Debug, ToSchema)]
pub struct LabelKey {
    pub label: Option<String>,
    pub key: Option<String>,
}

#[derive(Serialize, Debug, ToSchema)]
pub struct Geographic {
    pub province: Option<LabelKey>,
    pub city: Option<LabelKey>,
}

#[derive(Serialize, Debug, ToSchema)]
pub struct CurrentUser {
    pub name: Option<String>,
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
    pub geographic: Option<Geographic>,
    pub address: Option<String>,
    pub phone: Option<String>,
}

#[derive(Serialize, Debug, ToSchema)]
pub struct NoLogintUser {
    #[serde(rename(serialize = "isLogin"))]
    #[schema(rename = "isLogin")]
    pub login: bool,
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

#[derive(Serialize, Debug, ToSchema)]
pub struct UserRespone<T> {
    pub data: T,
    #[serde(rename(serialize = "errorCode"))]
    #[schema(rename = "errorCode")]
    pub error_code: i32,
    #[serde(rename(serialize = "errorMessage"))]
    #[schema(rename = "errorMessage")]
    pub error_message: String,
    pub success: bool,
}
