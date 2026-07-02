//! Evidence snapshot wire types — the structured read-only evidence a set of
//! collectors produces for one diagnosis.
//!
//! A snapshot is a list of [`EvidenceEntry`] (one per capability), each carrying
//! the [`AgentOutcome`] verbatim plus context-budget metadata. It is the wire
//! payload a thin edge (被控端 B) ships back to the central orchestrator (A) in
//! response to a remote-collect request, and the replay unit for offline eval.
//!
//! This type lives in the protocol crate (not the server) so both the edge that
//! produces it and the central brain that consumes it share one definition and
//! cannot drift. Screenshots are carried as a model-ready data URL
//! ([`EvidenceEntry::image_data_url`]) produced by the edge — the raw image
//! bytes never travel to A.

use serde::{Deserialize, Serialize};

use crate::{AgentOutcome, Capability};

/// Schema version of the [`EvidenceSnapshot`] structure. Bump when the snapshot
/// shape changes so eval regressions and the audit trail can attribute a result
/// to the evidence schema that produced it.
pub const EVIDENCE_SCHEMA_VERSION: &str = "evidence-v1";

fn default_evidence_schema_version() -> String {
    EVIDENCE_SCHEMA_VERSION.to_string()
}

/// One captured read context within a snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvidenceEntry {
    /// Dotted capability name (e.g. `process.list`).
    pub capability: String,
    /// The collector outcome, captured verbatim so replay is byte-identical to
    /// what the edge would have shipped.
    pub outcome: AgentOutcome,
    /// Names of fields scrubbed before this evidence is shown to a model.
    #[serde(default)]
    pub redactions: Vec<String>,
    /// Serialized JSON size of `outcome`, in bytes — the per-capability context
    /// budget input.
    pub size_bytes: usize,
    /// Model-ready screenshot data URL (`data:image/jpeg;base64,...`), set only
    /// for the `screen.capture.current` entry when the edge has refit the image
    /// for the model. The contract is **the edge produces this string and the
    /// central brain passes it straight into the provider vision message** — A
    /// never touches raw image bytes. `None` for non-screen entries and for
    /// snapshots whose screenshot has not been refit (e.g. local eval fixtures).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_data_url: Option<String>,
}

/// A replayable bundle of evidence for one diagnosis.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvidenceSnapshot {
    /// Stable scenario key (e.g. `high_cpu`) or `"live"` for a real capture.
    pub scenario: String,
    /// Human-readable description of the fault the snapshot represents.
    pub description: String,
    /// RFC3339 capture time. Fixed in committed fixtures so replay is
    /// deterministic.
    pub recorded_at: String,
    /// Schema version of this snapshot, defaulting to the current
    /// [`EVIDENCE_SCHEMA_VERSION`] for snapshots recorded before the field
    /// existed (`#[serde(default)]` keeps older fixtures replayable).
    #[serde(default = "default_evidence_schema_version")]
    pub schema_version: String,
    pub contexts: Vec<EvidenceEntry>,
}

impl EvidenceSnapshot {
    /// Build a snapshot from a set of `(capability, outcome)` pairs, computing
    /// each entry's serialized size. Redactions and `image_data_url` start
    /// empty.
    pub fn record(
        scenario: impl Into<String>,
        description: impl Into<String>,
        recorded_at: impl Into<String>,
        entries: Vec<(Capability, AgentOutcome)>,
    ) -> Self {
        let contexts = entries
            .into_iter()
            .map(|(cap, outcome)| {
                let size_bytes = serde_json::to_vec(&outcome).map(|v| v.len()).unwrap_or(0);
                EvidenceEntry {
                    capability: cap.as_str().to_string(),
                    outcome,
                    redactions: Vec::new(),
                    size_bytes,
                    image_data_url: None,
                }
            })
            .collect();
        EvidenceSnapshot {
            scenario: scenario.into(),
            description: description.into(),
            recorded_at: recorded_at.into(),
            schema_version: EVIDENCE_SCHEMA_VERSION.to_string(),
            contexts,
        }
    }

    /// Parse a snapshot from its JSON form (offline replay / chunk reassembly
    /// entry point).
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    /// Serialize to pretty JSON for committing as a fixture.
    pub fn to_json_pretty(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Total serialized evidence size across all contexts, in bytes.
    pub fn total_size_bytes(&self) -> usize {
        self.contexts.iter().map(|c| c.size_bytes).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        AgentError, AgentErrorKind, OperationOutput, ProcessEntry, ProcessListOutput,
        ReadContextOutput,
    };

    fn process_outcome() -> AgentOutcome {
        AgentOutcome::Ok(OperationOutput::ReadContext(
            ReadContextOutput::ProcessList(ProcessListOutput {
                processes: vec![ProcessEntry {
                    pid: 1,
                    name: "init".into(),
                    cpu_percent: 0.1,
                    memory_bytes: 1000,
                    user: None,
                    command_line_redacted: false,
                }],
                truncated: false,
            }),
        ))
    }

    #[test]
    fn record_computes_sizes_and_total() {
        let snap = EvidenceSnapshot::record(
            "live",
            "desc",
            "2026-06-16T00:00:00Z",
            vec![(Capability::ProcessList, process_outcome())],
        );
        assert_eq!(snap.contexts.len(), 1);
        assert!(snap.contexts[0].size_bytes > 0);
        assert_eq!(snap.total_size_bytes(), snap.contexts[0].size_bytes);
        assert!(snap.contexts[0].image_data_url.is_none());
    }

    #[test]
    fn round_trips_through_json() {
        let snap = EvidenceSnapshot::record(
            "live",
            "desc",
            "2026-06-16T00:00:00Z",
            vec![(Capability::ProcessList, process_outcome())],
        );
        let json = snap.to_json_pretty().unwrap();
        let back = EvidenceSnapshot::from_json(&json).unwrap();
        assert_eq!(snap, back);
    }

    #[test]
    fn image_data_url_round_trips_and_omits_when_none() {
        let mut snap = EvidenceSnapshot::record(
            "live",
            "desc",
            "2026-06-16T00:00:00Z",
            vec![(Capability::ScreenCaptureCurrent, process_outcome())],
        );
        // None is skipped from the serialized form.
        assert!(!snap.to_json_pretty().unwrap().contains("image_data_url"));
        snap.contexts[0].image_data_url = Some("data:image/jpeg;base64,AAAA".into());
        let json = snap.to_json_pretty().unwrap();
        assert!(json.contains("image_data_url"));
        let back = EvidenceSnapshot::from_json(&json).unwrap();
        assert_eq!(
            back.contexts[0].image_data_url.as_deref(),
            Some("data:image/jpeg;base64,AAAA")
        );
    }

    #[test]
    fn legacy_snapshot_without_schema_version_defaults() {
        let json = r#"{"scenario":"x","description":"d","recorded_at":"t","contexts":[]}"#;
        let loaded = EvidenceSnapshot::from_json(json).expect("legacy snapshot must parse");
        assert_eq!(loaded.schema_version, EVIDENCE_SCHEMA_VERSION);
    }

    #[test]
    fn mixed_outcomes_round_trip() {
        let err = AgentOutcome::Err(AgentError {
            kind: AgentErrorKind::InvalidInput,
            message: "no such container".into(),
            retryable: false,
            safe_for_model: true,
            error_code: None,
        });
        let snap = EvidenceSnapshot::record(
            "mixed",
            "d",
            "t",
            vec![
                (Capability::ProcessList, process_outcome()),
                (Capability::ContainerInspect, err),
            ],
        );
        let json = snap.to_json_pretty().unwrap();
        let back = EvidenceSnapshot::from_json(&json).unwrap();
        assert_eq!(snap, back);
        assert!(
            back.contexts
                .iter()
                .any(|c| matches!(c.outcome, AgentOutcome::Err(_)))
        );
    }
}
