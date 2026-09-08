//! Pure validation of the server-owned, revisioned budget ceiling.
use desk_agent_protocol::{
    capability_grant::MAX_GRANT_USES,
    schedule::{contract::TaskBudget, policy::ScheduleBudgetPolicy},
};

pub fn initial() -> ScheduleBudgetPolicy {
    ScheduleBudgetPolicy {
        schema_version: 1,
        revision: 0,
        maximum: super::TASK_PUBLICATION_BUDGET,
    }
}

pub fn valid_budget(budget: &TaskBudget) -> bool {
    (1..=MAX_GRANT_USES).contains(&budget.max_runs_per_utc_day)
        && (1..=MAX_GRANT_USES).contains(&budget.max_calls_per_run)
        && (1..=i64::MAX as u64).contains(&budget.max_model_tokens_per_run)
        && (1..=86_400).contains(&budget.max_runtime_seconds)
}

pub fn validate(policy: &ScheduleBudgetPolicy) -> Result<(), &'static str> {
    if policy.schema_version != 1
        || policy.revision > i64::MAX as u64
        || !valid_budget(&policy.maximum)
    {
        return Err("invalid schedule budget policy");
    }
    Ok(())
}

pub fn candidate(
    current: &ScheduleBudgetPolicy,
    maximum: TaskBudget,
) -> Result<ScheduleBudgetPolicy, &'static str> {
    validate(current)?;
    let next = ScheduleBudgetPolicy {
        schema_version: 1,
        revision: current
            .revision
            .checked_add(1)
            .ok_or("schedule policy revision exhausted")?,
        maximum,
    };
    validate(&next)?;
    Ok(next)
}

pub fn permits(policy: &ScheduleBudgetPolicy, budget: &TaskBudget) -> bool {
    validate(policy).is_ok()
        && valid_budget(budget)
        && budget.max_runs_per_utc_day <= policy.maximum.max_runs_per_utc_day
        && budget.max_calls_per_run <= policy.maximum.max_calls_per_run
        && budget.max_model_tokens_per_run <= policy.maximum.max_model_tokens_per_run
        && budget.max_runtime_seconds <= policy.maximum.max_runtime_seconds
}
