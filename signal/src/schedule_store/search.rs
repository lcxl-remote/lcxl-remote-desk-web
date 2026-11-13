//! Owner-scoped task search.
use super::{ScheduleStore, ScheduleStoreError, entity};

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
        desk_assistant_data_store::schedule::search(
            &self.db,
            owner,
            after,
            limit,
            kind,
            status,
            title,
            device,
            conversation,
            attention,
        )
        .await
    }
}
