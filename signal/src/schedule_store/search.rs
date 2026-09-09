//! Owner-scoped filters run before pagination, including pending execution approval.
use super::{ScheduleStore, ScheduleStoreError, entity};
use crate::entity::agent_schedule_run as run;
use sea_orm::{
    ColumnTrait, Condition, EntityTrait, PaginatorTrait, QueryFilter, QueryOrder, QuerySelect,
    sea_query::Query,
};
impl ScheduleStore {
    #[allow(clippy::too_many_arguments)]
    pub async fn search(
        &self,
        owner: i32,
        after: i64,
        limit: u64,
        kind: Option<&str>,
        status: Option<&str>,
        title: Option<&str>,
        device: Option<&str>,
        conversation: Option<&str>,
        attention: bool,
    ) -> Result<(Vec<entity::Model>, u64, u64), ScheduleStoreError> {
        if owner <= 0
            || after < 0
            || limit == 0
            || limit > 100
            || title.is_some_and(|value| value.len() > 256 || value.chars().any(char::is_control))
            || conversation.is_some_and(|value| {
                value.is_empty() || value.len() > 256 || value.chars().any(char::is_control)
            })
            || device.is_some_and(|value| value.is_empty() || value.len() > 256)
        {
            return Err(ScheduleStoreError::Invalid);
        }
        let mut query = entity::Entity::find()
            .filter(entity::Column::OwnerUserId.eq(owner))
            .filter(entity::Column::Status.is_not_in(["deleted", "pending_review"]));
        if let Some(kind) = kind {
            query = query.filter(entity::Column::Kind.eq(kind));
        }
        if let Some(status) = status {
            query = query.filter(entity::Column::Status.eq(status));
        }
        if let Some(device) = device {
            query = query.filter(entity::Column::TargetDeviceId.eq(device));
        }
        if let Some(conversation) = conversation {
            query = query.filter(entity::Column::SourceConversationId.eq(conversation));
        }
        if let Some(title) = title.filter(|value| !value.trim().is_empty()) {
            query = query.filter(entity::Column::Title.contains(title.trim()));
        }
        let pending = Condition::any()
            .add(entity::Column::Status.is_in(["draft", "awaiting_authorization", "paused"]))
            .add(
                entity::Column::ActiveRunId.in_subquery(
                    Query::select()
                        .column(run::Column::RunId)
                        .from(run::Entity)
                        .and_where(run::Column::OwnerUserId.eq(owner))
                        .and_where(run::Column::Status.eq("awaiting_permission"))
                        .to_owned(),
                ),
            );
        let attention_count = query
            .clone()
            .filter(pending.clone())
            .count(&self.db)
            .await?;
        if attention {
            query = query.filter(pending);
        }
        let total = query.clone().count(&self.db).await?;
        let rows = query
            .filter(entity::Column::Id.gt(after))
            .order_by_asc(entity::Column::Id)
            .limit(limit)
            .all(&self.db)
            .await?;
        Ok((rows, total, attention_count))
    }
}
