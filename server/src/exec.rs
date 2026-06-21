//! Server-side exec risk classification.
//!
//! The classifier itself (template whitelist, tokenizer, blocklist ordering) was
//! moved into `desk-diagnose-core` so the Direct daemon and the Manager
//! orchestrator classify a command identically — a single source of truth that
//! prevents the two runtimes' executable surfaces from drifting apart. This
//! module re-exports it under the historical `crate::exec` path used across the
//! server (the daemon confirm flow, the diagnose prompt builder, the Direct exec
//! seam).

pub use desk_agent_protocol::exec_policy::ExecLimits;
pub use desk_diagnose_core::exec_classify::{
    ClassifyOutcome, CommandForm, classify_command, classify_command_with, command_forms,
};
