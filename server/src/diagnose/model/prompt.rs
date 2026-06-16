//! Structured prompt assembly for the diagnose model call.
//!
//! The logic now lives in the shared `desk-diagnose-core` crate so the edge and
//! the central orchestrator assemble the prompt identically (and the screenshot
//! is attached from the edge-produced data URL, never decoded here). Re-exported
//! so existing `prompt::*` paths keep resolving.

pub use desk_diagnose_core::prompt::{
    ChatMessage, ChatRole, PROMPT_VERSION, ResponseFormatSpec, SYSTEM_PROMPT, build_messages,
    diagnosis_json_schema,
};
