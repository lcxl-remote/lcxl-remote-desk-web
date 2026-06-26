//! Model-side evidence post-processing for the diagnose capability path.
//!
//! AI diagnosis is orchestrated by the central signaling brain, so this host no
//! longer drives a model adapter. What remains is [`screenshot`], the
//! evidence-snapshot screenshot refit / strip used by the collector and the
//! `collect_for_remote` capability path before evidence travels to the central
//! brain.

pub mod screenshot;
