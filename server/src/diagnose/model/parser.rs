//! Parse the model's response text into a structured `Diagnosis`.
//!
//! The logic now lives in the shared `desk-diagnose-core` crate so the edge and
//! the central orchestrator parse model output identically. Re-exported here so
//! existing `parser::*` paths keep resolving.

pub use desk_diagnose_core::parser::{ParseOutcome, parse_diagnosis};
