//! Manage tasks from a model turn through the shared management rules.
#[cfg(test)]
use desk_assistant_data_store::schedule::{
    cancel_managed_task as cancel, list_managed_tasks as list, lock_target,
};

#[cfg(test)]
mod tests;
