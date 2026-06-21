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
//! - [`seam`]: the [`ModelSeam`](seam::ModelSeam) the loop calls, abstracting the
//!   wire dialect.

pub mod agent_loop;
pub mod agentic_prompt;
pub mod chat;
pub mod chunk;
pub mod exec_tools;
pub mod parser;
pub mod prompt;
pub mod read_tools;
pub mod registry;
pub mod seam;
pub mod selection;
pub mod session;
pub mod stream;
pub mod trim;

/// Default model context budget when `max_context_bytes` is unset (128 KB).
pub const DEFAULT_MAX_CONTEXT_BYTES: usize = 131_072;

/// Circuit-breaker bound on model→tool steps within a single turn before the loop
/// stops (turn-level: reset when a turn is claimed). Prevents a prompt-injected or
/// looping model from driving unbounded tool calls in one turn. The concrete value
/// is tuned against M1a measurement; the semantics (per-turn, not per-conversation)
/// are the contract.
pub const MAX_STEPS_PER_TURN: u32 = 6;

/// Circuit-breaker bound on how many times the **same** tool may be invoked within
/// a single turn, catching a model stuck re-calling one tool. Turn-level like
/// [`MAX_STEPS_PER_TURN`].
pub const MAX_SAME_TOOL_PER_TURN: u32 = 3;
