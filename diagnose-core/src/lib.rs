//! Model-agnostic Device Assistant logic shared by the thin edge and central brain.
//!
//! Splitting this out of `server` lets the manager run the same capability
//! selection, prompt assembly, response parsing, and evidence chunking the edge
//! used to run in-process, so the two sides can never drift. The crate is pure
//! logic: it depends only on `desk-agent-protocol` (wire types), serde, base64,
//! sha2, and async-trait (for the [`seam`] contract). Anything with a heavy or
//! platform dependency — screenshot re-encoding, the model transport adapters —
//! stays in `server`.
//!
//! - [`prompt`]: redacted evidence → neutral chat messages (text only; the edge
//!   has already turned any screenshot into a model-ready data URL).
//! - [`chunk`]: serialize / reassemble an [`desk_agent_protocol::evidence::EvidenceSnapshot`]
//!   across the chunked remote-collect response.
//! - [`chat`]: provider-agnostic chat / tool-calling contracts (messages, tools,
//!   model turn, stop-reason validation) the model adapters and agentic loop share.
//! - [`exec_classify`]: pure exec command classification (blocklist, tokenizer,
//!   template set, plus the explicit owner-interactive fallback) → a
//!   [`desk_agent_protocol::exec::CommandClassification`]
//!   plus a sealed plan draft, shared so both runtimes gate execution identically.
//! - [`seam`]: the [`ModelSeam`](seam::ModelSeam) the loop calls, abstracting the
//!   wire dialect.

pub mod agent_loop;
#[cfg(test)]
mod agent_loop_acceptance;
pub mod agentic_prompt;
pub mod browser_control;
pub mod capability_availability;
pub mod capability_grant;
pub mod capability_risk;
pub mod chat;
pub mod chunk;
pub mod communication;
pub mod content_safety;
pub mod context_attachment;
pub mod conversation_key;
pub mod data_policy;
pub mod device_assistant;
pub mod durable_action;
pub mod dynamic_run;
pub mod edge_registry;
pub mod exec_classify;
pub mod exec_tools;
pub mod image_input;
pub mod model_capability;
pub mod model_context;
pub mod model_egress;
pub mod model_profile;
pub mod parser;
pub mod permission_tools;
pub mod prompt;
pub mod provider_probe;
pub mod provider_registry;
pub mod read_tools;
pub mod redaction;
pub mod registry;
pub mod replay;
pub mod seam;
pub mod selection;
pub mod session;
pub mod simulated_grant;
pub mod sink_authorizer;
pub mod spreadsheet_formula;
pub mod stream;
pub mod task_status_tools;
pub mod terminal_complete;
pub mod terminal_copilot;
mod text_parse;
pub mod trim;
pub mod wait_tools;

/// Default model context budget when `max_context_bytes` is unset (128 KB).
///
/// This value is retained only for upgrading legacy OSS configurations. New and
/// edited profiles must provide an explicit value and must not silently use this
/// constant as a model capability default.
pub const DEFAULT_MAX_CONTEXT_BYTES: usize = 131_072;

/// Smallest application-supported model history budget (4 KiB).
pub const MIN_MODEL_CONTEXT_BYTES: usize = 4_096;

/// Largest application-supported model history budget (16 MiB).
///
/// This is a resource-safety ceiling, not a statement about any provider's token
/// context window.
pub const MAX_MODEL_CONTEXT_BYTES: usize = 16 * 1_024 * 1_024;

/// Default circuit-breaker bound on model reasoning rounds within a single turn.
///
/// One round is one model response and may request multiple tools; the final
/// answer also consumes a round. The counter resets when a new user turn is
/// claimed.
pub const MAX_STEPS_PER_TURN: u32 = 40;
pub const MIN_STEPS_PER_TURN: u32 = 1;
pub const MAX_STEPS_PER_TURN_LIMIT: u32 = 80;

/// Circuit-breaker bound on how many times the **same** tool may be invoked within
/// a single turn, catching a model stuck re-calling one tool. Turn-level like
/// [`MAX_STEPS_PER_TURN`]. Runtimes may expose an operator setting and pass a
/// value within [`MIN_SAME_TOOL_PER_TURN`]..=[`MAX_SAME_TOOL_PER_TURN_LIMIT`];
/// this constant is the fallback default.
pub const MAX_SAME_TOOL_PER_TURN: u32 = 20;
pub const MIN_SAME_TOOL_PER_TURN: u32 = 1;
pub const MAX_SAME_TOOL_PER_TURN_LIMIT: u32 = 50;
