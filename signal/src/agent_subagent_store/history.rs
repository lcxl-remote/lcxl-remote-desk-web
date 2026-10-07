//! Read all child history in bounded pages without a cumulative task limit.
use super::*;
use sea_orm::{DatabaseTransaction, PaginatorTrait};

pub(crate) async fn group_children_on(
    txn: &DatabaseTransaction,
    group: &DelegationGroup,
) -> Result<Vec<run_row::Model>, DbErr> {
    group.validate().map_err(|_| invalid())?;
    let mut pages = run_row::Entity::find()
        .filter(run_row::Column::GroupId.eq(&group.group_id))
        .filter(run_row::Column::RootConversationId.eq(&group.root_conversation_id))
        .filter(run_row::Column::ActorId.eq(&group.actor_id))
        .filter(run_row::Column::DeviceId.eq(&group.device_id))
        .order_by_asc(run_row::Column::ChildConversationId)
        .paginate(txn, 128);
    let mut children = Vec::new();
    while let Some(page) = pages.fetch_and_next().await? {
        children.extend(page);
    }
    Ok(children)
}
