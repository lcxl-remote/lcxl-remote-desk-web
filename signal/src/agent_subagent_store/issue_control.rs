//! Reuse native delegation control for affected issue requests.

use super::*;
use crate::config::connection::DatabaseTransaction;
use desk_assistant_data_store::issue_authority::settings::AffectedWork;
use desk_diagnose_core::{
    model_observability::permission::PendingEnds, subagent::DelegationSource,
};

pub(crate) async fn apply_issue_changes_on(
    txn: &DatabaseTransaction,
    actor: &str,
    subscription: &str,
    affected: &[AffectedWork],
    now_ms: i64,
) -> Result<PendingEnds, DbErr> {
    let mut permission_ends = PendingEnds::default();
    for work in affected {
        let row = group_row::Entity::find()
            .filter(group_row::Column::RootConversationId.eq(&work.execution_root_id))
            .filter(group_row::Column::ActorId.eq(actor))
            .filter(group_row::Column::SourceWorkbenchId.eq(&work.workbench_id))
            .filter(group_row::Column::DeviceId.eq(""))
            .one(txn)
            .await?;
        let Some(row) = row else {
            continue;
        };
        let group = decode_group(&row)?;
        let DelegationSource::IssueWork { source } = &group.source else {
            return Err(invalid());
        };
        if source.subscription_id != subscription || source.request_id != work.request_id {
            return Err(invalid());
        }
        if group.source_admission == SourceAdmission::Closed
            || group.source_admission == work.source_admission
        {
            continue;
        }
        permission_ends
            .extend(apply_group_admission_on(txn, &row, work.source_admission, now_ms).await?);
    }
    Ok(permission_ends)
}
