//! The model seam: the agentic loop's abstraction over a model call.
//!
//! The loop (built on this crate) never talks a provider's wire dialect. It
//! hands a [`ModelRequest`] (conversation + advertised tools + steering) to a
//! [`ModelSeam`] and gets back a normalized [`ModelTurn`]. Each runtime supplies
//! its own implementation: the Direct runtime wraps the OpenAI/Anthropic
//! streaming adapters; the Manager runtime wraps its model dialect. Both map onto
//! the same neutral types here, so the two sides can never drift.
//!
//! `?Send`: the Direct adapters use `awc` (`!Send`) on actix's single-threaded
//! runtime, and the manager awaits the model call inline, so a non-`Send` future
//! works for both. The bound is documented here so future implementers don't
//! accidentally require `Send`.

use async_trait::async_trait;
use desk_agent_protocol::AgentError;

use crate::chat::{ChatMessage, ModelTurn, ToolChoice, ToolSpec};
use crate::prompt::ResponseFormatSpec;

/// A model request in neutral terms: the conversation, the tools the model may
/// call, how it is steered toward them, and the requested response format. The
/// wire shape (OpenAI vs Anthropic) is the [`ModelSeam`] implementation's concern.
#[derive(Debug, Clone)]
pub struct ModelRequest {
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ToolSpec>,
    pub tool_choice: ToolChoice,
    pub response_format: ResponseFormatSpec,
}

impl ModelRequest {
    /// A tool-free request (the single-turn diagnose shape): no tools advertised,
    /// the model is free to answer in text.
    pub fn text_only(messages: Vec<ChatMessage>, response_format: ResponseFormatSpec) -> Self {
        Self {
            messages,
            tools: Vec::new(),
            tool_choice: ToolChoice::Auto,
            response_format,
        }
    }
}

/// Receives streaming output from a model turn as it arrives.
///
/// Text deltas are **provisional** until the turn's [`StopReason`] is known: the
/// loop commits them only on a final answer (`EndTurn`) and discards them
/// otherwise, so intermediate tool-calling turns never leak half-text to the UI.
/// Object-safe so a `&mut dyn TurnSink` can be passed across the seam.
///
/// [`StopReason`]: crate::chat::StopReason
pub trait TurnSink {
    /// An incremental fragment of the assistant's text for the current turn.
    fn on_text_delta(&mut self, delta: &str);
}

/// A sink that ignores all streamed output (for non-streaming callers / tests).
pub struct NullTurnSink;

impl TurnSink for NullTurnSink {
    fn on_text_delta(&mut self, _delta: &str) {}
}

/// The model call, abstracted from the wire dialect. Implementations stream text
/// deltas through `sink` and return the fully assembled, normalized [`ModelTurn`]
/// (text + tool calls + stop reason + usage).
#[async_trait(?Send)]
pub trait ModelSeam {
    async fn call(
        &self,
        request: ModelRequest,
        sink: &mut dyn TurnSink,
    ) -> Result<ModelTurn, AgentError>;
}
