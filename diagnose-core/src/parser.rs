//! Parse the model's response text into a structured [`Diagnosis`].
//!
//! The model is asked for a bare JSON object, but real models wrap it in prose
//! or ```json fences. The parser extracts the outermost JSON object and
//! deserializes it; the diagnose DTOs already match the requested schema, so a
//! well-formed response maps directly. A response that cannot be parsed
//! **degrades** rather than failing: the raw text becomes the summary with a
//! low confidence and a `missing_info` note, so the operator still sees the
//! model's answer.

use desk_agent_protocol::diagnose::{Confidence, Diagnosis};

/// Whether [`parse_diagnosis`] obtained a structured result or had to degrade.
///
/// The function always returns a usable [`Diagnosis`], so "produced a diagnosis"
/// is vacuously true and cannot serve as a quality signal. This outcome makes the
/// distinction observable: it is stamped into the `ai.model.responded` audit
/// (`parse=structured|degraded`) and counted by the offline eval harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseOutcome {
    /// The response carried a well-formed JSON object matching the schema.
    Structured,
    /// The response was not parseable JSON; the raw text was kept as a
    /// low-confidence summary.
    Degraded,
}

impl ParseOutcome {
    /// Stable lowercase identifier used in audit summaries and eval counters.
    pub fn as_str(self) -> &'static str {
        match self {
            ParseOutcome::Structured => "structured",
            ParseOutcome::Degraded => "degraded",
        }
    }
}

/// Parse model output into a [`Diagnosis`], degrading on malformed JSON. The
/// returned `collected` list is always empty here — the orchestrator stamps the
/// authoritative value. The [`ParseOutcome`] reports whether the structured path
/// or the degraded fallback was taken.
pub fn parse_diagnosis(content: &str) -> (Diagnosis, ParseOutcome) {
    match extract_json_object(content).and_then(|json| serde_json::from_str::<Diagnosis>(json).ok())
    {
        Some(mut diagnosis) => {
            // The model does not own `collected`; the orchestrator overwrites it.
            diagnosis.collected.clear();
            (diagnosis, ParseOutcome::Structured)
        }
        None => (degraded(content), ParseOutcome::Degraded),
    }
}

/// A degraded diagnosis: keep the model's text as the summary, mark it
/// low-confidence, and note the structured parse failed.
fn degraded(content: &str) -> Diagnosis {
    const MAX_SUMMARY: usize = 4_000;
    let summary = truncate_on_char_boundary(content.trim(), MAX_SUMMARY);
    Diagnosis {
        summary,
        confidence: Confidence::Low,
        missing_info: vec!["The model response was not structured JSON; showing raw text.".into()],
        ..Default::default()
    }
}

/// Extract the outermost `{...}` JSON object from a response that may carry
/// surrounding prose or code fences. Returns `None` if no balanced object is
/// found.
pub(crate) fn extract_json_object(content: &str) -> Option<&str> {
    let start = content.find('{')?;
    // Walk from the first `{` tracking brace depth, ignoring braces inside
    // strings, to find the matching close.
    let bytes = content.as_bytes();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, &b) in bytes[start..].iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&content[start..=start + offset]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Truncate a string to at most `max` bytes without splitting a UTF-8 char.
pub(crate) fn truncate_on_char_boundary(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use desk_agent_protocol::RiskLevel;

    const WELL_FORMED: &str = r#"{
        "summary": "Port 8080 is held by old-api.exe.",
        "confidence": "high",
        "findings": [
            {"title": "Port conflict", "evidence_refs": ["network.ports[0]"], "explanation": "old-api.exe holds 8080."}
        ],
        "commands": [
            {"shell": "powershell", "command": "Get-NetTCPConnection -LocalPort 8080", "purpose": "Find owner", "risk": "low", "requires_confirmation": false}
        ],
        "next_steps": ["Stop the stale service"],
        "missing_info": []
    }"#;

    /// A schema-conformant response maps directly to the DTOs.
    #[test]
    fn parses_well_formed_json() {
        let (d, outcome) = parse_diagnosis(WELL_FORMED);
        assert_eq!(outcome, ParseOutcome::Structured);
        assert_eq!(d.confidence, Confidence::High);
        assert_eq!(d.summary, "Port 8080 is held by old-api.exe.");
        assert_eq!(d.findings.len(), 1);
        assert_eq!(d.findings[0].evidence_refs, vec!["network.ports[0]"]);
        assert_eq!(d.commands.len(), 1);
        assert_eq!(d.commands[0].risk, RiskLevel::Low);
        assert_eq!(d.next_steps, vec!["Stop the stale service"]);
    }

    /// JSON wrapped in a ```json fence and prose is still extracted.
    #[test]
    fn parses_json_in_code_fence_with_prose() {
        let wrapped = format!("Here is my analysis:\n```json\n{WELL_FORMED}\n```\nHope it helps!");
        let (d, outcome) = parse_diagnosis(&wrapped);
        assert_eq!(outcome, ParseOutcome::Structured);
        assert_eq!(d.confidence, Confidence::High);
        assert_eq!(d.commands.len(), 1);
    }

    /// A nested object with braces in strings is balanced correctly.
    #[test]
    fn handles_braces_inside_strings() {
        let json = r#"prefix {"summary": "use {placeholder} here", "confidence": "medium"} suffix"#;
        let (d, outcome) = parse_diagnosis(json);
        assert_eq!(outcome, ParseOutcome::Structured);
        assert_eq!(d.confidence, Confidence::Medium);
        assert_eq!(d.summary, "use {placeholder} here");
    }

    /// Non-JSON output degrades to a low-confidence summary of the raw text.
    #[test]
    fn degrades_on_non_json() {
        let (d, outcome) = parse_diagnosis("I think the CPU is just busy, no JSON here.");
        assert_eq!(outcome, ParseOutcome::Degraded);
        assert_eq!(d.confidence, Confidence::Low);
        assert!(d.summary.contains("CPU is just busy"));
        assert!(!d.missing_info.is_empty());
    }

    /// Malformed JSON (right shape, wrong syntax) also degrades.
    #[test]
    fn degrades_on_malformed_json() {
        let (d, outcome) = parse_diagnosis(r#"{"summary": "x", "confidence": }"#);
        assert_eq!(outcome, ParseOutcome::Degraded);
        assert_eq!(d.confidence, Confidence::Low);
        assert!(!d.missing_info.is_empty());
    }

    /// The parser never trusts a model-supplied `collected` list.
    #[test]
    fn clears_model_supplied_collected() {
        let with_collected =
            r#"{"summary": "x", "confidence": "low", "collected": ["forged.cap"]}"#;
        let (d, outcome) = parse_diagnosis(with_collected);
        assert_eq!(outcome, ParseOutcome::Structured);
        assert!(d.collected.is_empty());
    }
}
