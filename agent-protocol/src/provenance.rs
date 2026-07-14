//! Machine-readable provenance for AI-generated content, shared across the AI
//! output surfaces (diagnosis, Terminal Copilot, command completion).
//!
//! This is attached to the browser-facing content frames, not to the internal
//! model-adapter result: that result is flattened to a plain string before it
//! reaches the wire, so a marking placed there would never arrive. Carrying the
//! marking on the content frame lets the control end label AI-generated text and
//! lets exports carry a machine-readable marking, in line with the EU AI Act
//! Article 50(2) transparency obligation.
//!
//! Presence of this value is not what makes content "AI-generated": the content
//! frame's kind already establishes that. This carries the optional metadata
//! (which model, when, under which marking scheme). Consumers treat an AI content
//! frame as AI-generated even when this is absent (fail-closed), so a missing or
//! stripped value never downgrades content to "not AI".
//!
//! Timestamps are RFC3339 strings, emitter-stamped, so this crate stays free of a
//! `chrono` dependency (matching `audit` and `evidence`).

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use wincode::{SchemaRead, SchemaWrite};

/// Identifier of the marking scheme this crate currently emits. Bump when the
/// marking convention changes so detectors can tell versions apart.
pub const AI_MARKING_SCHEME_V1: &str = "lcxl-ai-provenance/1";

/// Machine-readable marking for a unit of AI-generated content.
#[derive(
    Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, SchemaWrite, SchemaRead, ToSchema,
)]
pub struct AiProvenance {
    /// The model that produced the content, if known (emitter-stamped). Free-form
    /// model name (e.g. `gpt-4o`), not a database row id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    /// RFC3339 timestamp of when the content was generated; emitter-stamped (this
    /// crate stays free of a clock dependency).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generated_at: Option<String>,
    /// Which marking scheme was applied, for forward compatibility as marking
    /// matures (e.g. towards watermarking). Emitters set [`AI_MARKING_SCHEME_V1`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marking_scheme: Option<String>,
}

impl AiProvenance {
    /// A provenance stamp for content generated now by `model_id` under the
    /// current marking scheme. `generated_at` is an RFC3339 string the caller
    /// stamps (this crate avoids a clock dependency); pass `None` when the
    /// emitter has no clock handy — the content is still marked AI by its frame.
    pub fn stamp(model_id: Option<String>, generated_at: Option<String>) -> Self {
        Self {
            model_id,
            generated_at,
            marking_scheme: Some(AI_MARKING_SCHEME_V1.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wincode::config::{Configuration, PREALLOCATION_SIZE_LIMIT_DISABLED};

    fn unbounded_config() -> Configuration<true, PREALLOCATION_SIZE_LIMIT_DISABLED> {
        Configuration::new()
    }

    #[test]
    fn stamp_sets_current_marking_scheme() {
        let p = AiProvenance::stamp(Some("gpt-4o".into()), Some("2026-07-14T00:00:00Z".into()));
        assert_eq!(p.model_id.as_deref(), Some("gpt-4o"));
        assert_eq!(p.generated_at.as_deref(), Some("2026-07-14T00:00:00Z"));
        assert_eq!(p.marking_scheme.as_deref(), Some(AI_MARKING_SCHEME_V1));
    }

    #[test]
    fn round_trips_through_json_and_wincode() {
        let p = AiProvenance::stamp(
            Some("claude-opus".into()),
            Some("2026-07-14T12:00:00Z".into()),
        );

        let json = serde_json::to_string(&p).expect("json encode");
        let back: AiProvenance = serde_json::from_str(&json).expect("json decode");
        assert_eq!(p, back);

        let config = unbounded_config();
        let bytes = wincode::config::serialize(&p, config).expect("wincode encode");
        let back2: AiProvenance =
            wincode::config::deserialize(&bytes, config).expect("wincode decode");
        assert_eq!(p, back2);
    }

    /// A default (empty) provenance round-trips: all fields are optional, so a
    /// content frame can carry a bare marking without model / timestamp.
    #[test]
    fn empty_provenance_round_trips_and_omits_optional_fields() {
        let p = AiProvenance::default();
        let json = serde_json::to_string(&p).expect("json encode");
        // All fields skip when None, so an empty stamp serializes to `{}`.
        assert_eq!(json, "{}");
        let back: AiProvenance = serde_json::from_str(&json).expect("json decode");
        assert_eq!(p, back);

        let config = unbounded_config();
        let bytes = wincode::config::serialize(&p, config).expect("wincode encode");
        let back2: AiProvenance =
            wincode::config::deserialize(&bytes, config).expect("wincode decode");
        assert_eq!(p, back2);
    }
}
