//! Fixed-step progress is derived from original dispatch receipts in this run.
use super::*;
use desk_agent_protocol::schedule::contract::TaskStepStatus;

pub(super) async fn states(
    txn: &DatabaseTransaction,
    authority: &schedule_store::CurrentTaskAuthority,
    budgets: &[ledger::Model],
) -> Result<BTreeMap<String, TaskStepStatus>, DbErr> {
    states_for(
        txn,
        &authority.run().conversation_id,
        authority.contract(),
        authority.provenance(),
        budgets,
    )
    .await
}

pub(super) async fn historical_states(
    txn: &DatabaseTransaction,
    context: &schedule_store::TaskReceiptContext,
    budgets: &[ledger::Model],
) -> Result<BTreeMap<String, TaskStepStatus>, DbErr> {
    states_for(
        txn,
        context.run_id(),
        context.contract(),
        context.provenance(),
        budgets,
    )
    .await
}

async fn states_for(
    txn: &DatabaseTransaction,
    run_id: &str,
    contract: &desk_diagnose_core::schedule::contract::ValidatedTaskContract,
    provenance: &desk_agent_protocol::capability_grant::TaskGrantProvenance,
    budgets: &[ledger::Model],
) -> Result<BTreeMap<String, TaskStepStatus>, DbErr> {
    let parent_json = serde_json::to_string(provenance).map_err(|_| invalid())?;
    let parent_digest = format!("{:x}", Sha256::digest(parent_json.as_bytes()));
    if budgets.iter().any(|budget| {
        budget.run_id != run_id
            || budget.kind != "tool_call"
            || budget.authority_sha256 != parent_digest
            || budget.reserved_units != 1
            || !contract
                .contract()
                .permissions
                .iter()
                .any(|rule| budget.rule_id.as_deref() == Some(rule.rule_id.as_str()))
    }) {
        return Err(invalid());
    }
    let works = agent_action_item::Entity::find()
        .filter(agent_action_item::Column::ConversationId.eq(run_id))
        .filter(agent_action_item::Column::Kind.eq(CAPABILITY_WORK_KIND))
        .all(txn)
        .await?;
    let mut result = BTreeMap::new();
    for step in &contract.contract().steps {
        let calls: Vec<_> = budgets
            .iter()
            .filter(|budget| budget.rule_id.as_deref() == Some(step.rule_id.as_str()))
            .collect();
        let state = match calls.as_slice() {
            [] => TaskStepStatus::Pending,
            [budget] => {
                let matching: Vec<_> = works
                    .iter()
                    .filter(|work| {
                        let grant = identity(run_id, &work.tool_call_id);
                        format!("{:x}", Sha256::digest(grant.as_bytes()))
                            == budget.logical_key_sha256
                    })
                    .collect();
                let [work] = matching.as_slice() else {
                    return Err(invalid());
                };
                if work.draft_hash != budget.input_sha256 {
                    return Err(invalid());
                }
                let dispatch = agent_capability_dispatch_outbox::Entity::find()
                    .filter(agent_capability_dispatch_outbox::Column::WorkId.eq(work.id))
                    .one(txn)
                    .await?;
                if let Some(dispatch) = dispatch {
                    if dispatch.computer_binding_json.is_some()
                        && SignalCapabilityGrantStore::observe_completed_task_provider_on(
                            txn,
                            &dispatch.dispatch_id,
                            provenance,
                        )
                        .await?
                        .is_some()
                    {
                        TaskStepStatus::Succeeded
                    } else {
                        TaskStepStatus::OutcomeUnknown
                    }
                } else {
                    TaskStepStatus::Prepared
                }
            }
            _ => return Err(invalid()),
        };
        result.insert(step.step_id.clone(), state);
    }
    Ok(result)
}
