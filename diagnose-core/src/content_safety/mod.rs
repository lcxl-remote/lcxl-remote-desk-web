//! Pure content-safety policy, prompt, parser, and runtime seam contracts.
//!
//! This module deliberately contains no HTTP, database, billing, or deployment
//! configuration. The manager supplies those concerns; the open-source signal
//! runtime does not configure or call this seam.

#[cfg(any(test, feature = "content-safety-eval"))]
pub mod eval;
pub mod parser;
pub mod policy;
pub mod prompt;
pub mod refusal;
pub mod seam;

pub use parser::parse_safety_verdict;
pub use policy::{aggregate_decision, category_decision};
pub use prompt::{
    CONTENT_SAFETY_PROMPT_VERSION, SafetyPrompt, build_image_prompt, build_input_prompt,
    build_model_turn_prompt,
};
pub use refusal::{RefusalReasonKey, refusal_placeholder_for, refusal_reason_for};
pub use seam::{
    ContentSafetyMode, ContentSafetySeam, SafetyContext, SafetyImage, SafetyInput, SafetyModelTurn,
    SafetyToolCall, SafetyVerdict, content_blocked_error, content_safety_image_unsupported,
    content_safety_unavailable, normalize_safety_error,
};
