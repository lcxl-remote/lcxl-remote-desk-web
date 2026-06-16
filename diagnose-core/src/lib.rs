//! Model-agnostic diagnose logic shared by the thin edge (被控端 B) and the
//! central orchestrator (A = manager).
//!
//! Splitting this out of `server` lets the manager run the same capability
//! selection, prompt assembly, response parsing, and evidence chunking the edge
//! used to run in-process, so the two sides can never drift. The crate is pure
//! logic: it depends only on `desk-agent-protocol` (wire types) + serde + base64
//! + sha2. Anything with a heavy or platform dependency — screenshot
//! re-encoding, the model transport adapters — stays in `server`.
//!
//! - [`selection`]: which capabilities a diagnosis collects, gated by policy.
//! - [`parser`]: model response text → structured [`desk_agent_protocol::diagnose::Diagnosis`].
//! - [`prompt`]: redacted evidence → neutral chat messages (text only; the edge
//!   has already turned any screenshot into a model-ready data URL).
//! - [`chunk`]: serialize / reassemble an [`desk_agent_protocol::evidence::EvidenceSnapshot`]
//!   across the chunked remote-collect response.

pub mod chunk;
pub mod parser;
pub mod prompt;
pub mod selection;

/// Default model context budget when `max_context_bytes` is unset (128 KB).
pub const DEFAULT_MAX_CONTEXT_BYTES: usize = 131_072;
