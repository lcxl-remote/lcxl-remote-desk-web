//! Capability selection for a diagnosis.
//!
//! The logic now lives in the shared `desk-diagnose-core` crate so the edge and
//! the central orchestrator select capabilities identically. Re-exported here so
//! existing `super::selection::*` paths keep resolving.

pub use desk_diagnose_core::selection::{CollectionPolicy, context_input_for, select_capabilities};
