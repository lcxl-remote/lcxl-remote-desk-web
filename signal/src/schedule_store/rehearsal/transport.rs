//! Resolve a reserved client intent before entering the normal owner-authorized turn.
use super::*;
use desk_agent_protocol::device_assistant::DeviceAssistantAsk;

impl ScheduleStore {
    /// Latest includes cancelled attempts; an older successful run must not hide a newer one.
    pub async fn read_latest_rehearsal(
        &self,
        owner: i32,
        schedule_id: &str,
    ) -> Result<Option<rehearsal::Model>, ScheduleStoreError> {
        use sea_orm::QueryOrder;
        self.read(owner, schedule_id).await?;
        Ok(rehearsal::Entity::find()
            .filter(rehearsal::Column::OwnerUserId.eq(owner))
            .filter(rehearsal::Column::ScheduleId.eq(schedule_id))
            .order_by_desc(rehearsal::Column::Id)
            .one(&self.db)
            .await?)
    }

    pub async fn rehearsal_for_input(
        &self,
        owner: i32,
        device: &str,
        ask: &DeviceAssistantAsk,
    ) -> Result<Option<rehearsal::Model>, ScheduleStoreError> {
        let Some(client) = ask
            .conversation_id
            .as_deref()
            .filter(|id| id.starts_with("rehearsal_"))
        else {
            return Ok(None);
        };
        let row = rehearsal::Entity::find()
            .filter(rehearsal::Column::ClientConversationId.eq(client))
            .filter(rehearsal::Column::OwnerUserId.eq(owner))
            .filter(rehearsal::Column::TargetDeviceId.eq(device))
            .one(&self.db)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        if row.status != "pending"
            || row.started_at.is_some()
            || ask.question != row.prompt
            || ask.locale != row.locale
            || ask.client_message_id != format!("rehearsal:{}:input", row.rehearsal_id)
            || !ask.selected_attachment_ids.is_empty()
        {
            return Err(ScheduleStoreError::Conflict);
        }
        Ok(Some(row))
    }

    pub async fn finish_rehearsal_failure_for_session(
        &self,
        owner: i32,
        conversation: &str,
    ) -> Result<(), ScheduleStoreError> {
        let row = rehearsal::Entity::find()
            .filter(rehearsal::Column::ConversationId.eq(conversation))
            .filter(rehearsal::Column::OwnerUserId.eq(owner))
            .one(&self.db)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        self.finish_failed_rehearsal(owner, &row.rehearsal_id)
            .await?;
        Ok(())
    }

    pub async fn finish_rehearsal_termination_for_session(
        &self,
        owner: i32,
        conversation: &str,
    ) -> Result<(), ScheduleStoreError> {
        let row = rehearsal::Entity::find()
            .filter(rehearsal::Column::ConversationId.eq(conversation))
            .filter(rehearsal::Column::OwnerUserId.eq(owner))
            .one(&self.db)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        self.finish_terminal_rehearsal(owner, &row.rehearsal_id)
            .await?;
        Ok(())
    }

    pub async fn finish_rehearsal_answer_for_session(
        &self,
        owner: i32,
        conversation: &str,
        answer: &str,
    ) -> Result<(), ScheduleStoreError> {
        let row = rehearsal::Entity::find()
            .filter(rehearsal::Column::ConversationId.eq(conversation))
            .filter(rehearsal::Column::OwnerUserId.eq(owner))
            .one(&self.db)
            .await?
            .ok_or(ScheduleStoreError::NotFound)?;
        self.finish_answered_rehearsal(owner, &row.rehearsal_id, answer)
            .await?;
        Ok(())
    }
}
