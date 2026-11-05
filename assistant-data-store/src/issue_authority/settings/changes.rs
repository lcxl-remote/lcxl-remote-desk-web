//! Authority changes are recorded on the original request. Dispatch still uses
//! the intersection of the frozen admission and the live policy.

use super::*;
use desk_agent_protocol::ai_assistant::issue_subscription::{
    IssueAuthorizationChange, IssueSourceAccess,
};
use desk_diagnose_core::subagent::group::SourceAdmission;
use sea_orm::Condition;

pub struct AffectedWork {
    pub execution_root_id: String,
    pub workbench_id: String,
    pub request_id: String,
    pub source_admission: SourceAdmission,
}

fn access(
    admitted: &IssueAdmission,
    policy: &IssueSubscription,
    now: i64,
) -> (IssueSourceAccess, bool, bool) {
    if admitted.usable(policy, now) {
        return (
            IssueSourceAccess::Open,
            admitted.can_use_devices(policy, now),
            admitted.can_publish(policy, now),
        );
    }
    let source_access = if !policy.enabled {
        let mut enabled = policy.clone();
        enabled.enabled = true;
        if admitted.usable(&enabled, now) {
            IssueSourceAccess::Paused
        } else {
            IssueSourceAccess::Withdrawn
        }
    } else {
        IssueSourceAccess::Withdrawn
    };
    (source_access, false, false)
}

pub(super) async fn record_on(
    db: &impl ConnectionTrait,
    previous: &IssueSubscription,
    policy: &IssueSubscription,
    now: i64,
) -> Result<Vec<AffectedWork>, AssistantStoreError> {
    let rows = admission::Entity::find()
        .filter(admission::Column::ActorId.eq(&policy.actor_id))
        .filter(admission::Column::SubscriptionId.eq(&policy.subscription_id))
        .filter(admission::Column::StoppedAtMs.is_null())
        .filter(admission::Column::RetiredAtMs.is_null())
        .filter(
            Condition::any()
                .add(admission::Column::ContentExpiresAtMs.is_null())
                .add(admission::Column::ContentExpiresAtMs.gt(now)),
        )
        .order_by_asc(admission::Column::Id)
        .all(db)
        .await?;
    let mut affected = Vec::new();
    for row in rows {
        let admitted = decode_admission(&row)?;
        let old = access(&admitted, previous, now);
        let current = access(&admitted, policy, now);
        if old == current {
            continue;
        }
        let change = IssueAuthorizationChange {
            revision: policy.revision,
            changed_at_ms: now,
            source_access: current.0,
            device_access: current.1,
            auto_reply: current.2,
        };
        let json = serde_json::to_string(&change).map_err(|_| invalid())?;
        let updated = admission::Entity::update_many()
            .filter(admission::Column::Id.eq(row.id))
            .filter(admission::Column::AuthorizationChangeJson.eq(row.authorization_change_json))
            .set(admission::ActiveModel {
                authorization_change_json: Set(Some(json)),
                ..Default::default()
            })
            .exec(db)
            .await?;
        if updated.rows_affected != 1 {
            return Err(AssistantStoreError::Inaccessible);
        }
        if (old.0 == IssueSourceAccess::Open) != (current.0 == IssueSourceAccess::Open) {
            affected.push(AffectedWork {
                execution_root_id: admitted.execution_root_id,
                workbench_id: admitted.subscription.conversation_id,
                request_id: admitted.request_id,
                source_admission: if current.0 == IssueSourceAccess::Open {
                    SourceAdmission::Open
                } else {
                    SourceAdmission::Paused
                },
            });
        }
    }
    Ok(affected)
}

pub(super) async fn read_on(
    db: &impl ConnectionTrait,
    actor: &str,
    subscription: &str,
    request: &str,
) -> Result<Option<IssueAuthorizationChange>, AssistantStoreError> {
    let json = admission::Entity::find()
        .select_only()
        .column(admission::Column::AuthorizationChangeJson)
        .filter(admission::Column::ActorId.eq(actor))
        .filter(admission::Column::SubscriptionId.eq(subscription))
        .filter(admission::Column::RequestId.eq(request))
        .filter(admission::Column::RetiredAtMs.is_null())
        .into_tuple::<Option<String>>()
        .one(db)
        .await?
        .flatten();
    json.map(|json| serde_json::from_str(&json).map_err(|_| invalid()))
        .transpose()
}
