//! Model-agnostic diagnose logic shared by the thin edge (被控端 B) and the
//! central orchestrator (A = manager).
//!
//! Splitting this out of `server` lets the manager run the same capability
//! selection, prompt assembly, response parsing, and evidence chunking the edge
//! used to run in-process, so the two sides can never drift. The crate is pure
//! logic: it depends only on `desk-agent-protocol` (wire types), serde, base64,
//! sha2, and async-trait (for the [`seam`] contract). Anything with a heavy or
//! platform dependency — screenshot re-encoding, the model transport adapters —
//! stays in `server`.
//!
//! - [`selection`]: which capabilities a diagnosis collects, gated by policy.
//! - [`parser`]: model response text → structured [`desk_agent_protocol::diagnose::Diagnosis`].
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
pub mod chat;
pub mod chunk;
pub mod conversation_key;
pub mod exec_classify;
pub mod exec_tools;
pub mod image_input;
pub mod model_capability;
pub mod parser;
pub mod prompt;
pub mod provider_probe;
pub mod read_tools;
pub mod redaction;
pub mod registry;
pub mod seam;
pub mod selection;
pub mod session;
pub mod stream;
pub mod terminal_complete;
pub mod terminal_copilot;
pub mod trim;
pub mod wait_tools;

/// Default model context budget when `max_context_bytes` is unset (128 KB).
pub const DEFAULT_MAX_CONTEXT_BYTES: usize = 131_072;

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
