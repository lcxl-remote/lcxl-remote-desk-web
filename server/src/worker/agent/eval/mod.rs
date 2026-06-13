//! Offline eval foundation for the AI read path.
//!
//! M1a has no model yet, so to make M1b's diagnosis quality measurable without
//! a live machine we freeze **evidence snapshots**: the structured output a set
//! of read collectors would produce for a given situation, serialized to JSON
//! and replayable offline. M1b's model / prompt eval runs against these fixed
//! snapshots instead of an expensive end-to-end run, so a prompt change is
//! validated by re-running the eval rather than reproducing a live fault.
//!
//! A snapshot is a list of [`EvidenceEntry`] — one per capability — each
//! carrying the [`AgentOutcome`] verbatim, a `redactions` placeholder (empty in
//! M1a; scrubbing lands in M1b), and the serialized size used for context-budget
//! calibration. Three acceptance scenarios (high CPU, an occupied port, a
//! failed container) are recorded as committed JSON fixtures and replayed by the
//! tests below.
//!
//! The companion measurement harness lives in [`measure`].

pub mod measure;

use desk_agent_protocol::{
    AgentError, AgentErrorKind, AgentOutcome, Capability, ContainerLogsOutput, ContainerSummary,
    CpuInfo, DiskInfo, MemoryInfo, OperationOutput, PortEntry, ProcessEntry, ProcessListOutput,
    ReadContextOutput, SystemInfoOutput,
};
use serde::{Deserialize, Serialize};

/// One captured read context within a snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvidenceEntry {
    /// Dotted capability name (e.g. `process.list`).
    pub capability: String,
    /// The collector outcome, captured verbatim so replay is byte-identical to
    /// what the daemon would have shipped.
    pub outcome: AgentOutcome,
    /// Names of fields that would be scrubbed before this evidence is shown to a
    /// model. Empty in M1a — the placeholder reserves the shape so M1b's
    /// redaction pass populates it without a snapshot format change.
    #[serde(default)]
    pub redactions: Vec<String>,
    /// Serialized JSON size of `outcome`, in bytes — the per-capability context
    /// budget input for M1b.
    pub size_bytes: usize,
}

/// A replayable bundle of evidence for one diagnosis scenario.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvidenceSnapshot {
    /// Stable scenario key (e.g. `high_cpu`).
    pub scenario: String,
    /// Human-readable description of the fault the snapshot represents.
    pub description: String,
    /// RFC3339 capture time. Fixed in committed fixtures so replay is
    /// deterministic.
    pub recorded_at: String,
    pub contexts: Vec<EvidenceEntry>,
}

impl EvidenceSnapshot {
    /// Build a snapshot from a set of `(capability, outcome)` pairs, computing
    /// each entry's serialized size. Redactions start empty (M1a).
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
                }
            })
            .collect();
        EvidenceSnapshot {
            scenario: scenario.into(),
            description: description.into(),
            recorded_at: recorded_at.into(),
            contexts,
        }
    }

    /// Parse a snapshot from its JSON form (offline replay entry point).
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

// ----------------------- scenario builders -----------------------
//
// These construct the recorded evidence in code so the committed fixtures are
// real serde output (correct nested-tag shape), not hand-authored JSON. The
// `generate_fixtures` test re-emits the JSON files from these builders.

const RECORDED_AT: &str = "2026-06-12T00:00:00+00:00";

fn ok(output: OperationOutput) -> AgentOutcome {
    AgentOutcome::Ok(output)
}

fn read(output: ReadContextOutput) -> OperationOutput {
    OperationOutput::ReadContext(output)
}

/// A runaway process pinning the CPU.
pub fn scenario_high_cpu() -> EvidenceSnapshot {
    let system_info = read(ReadContextOutput::SystemInfo(SystemInfoOutput {
        hostname: "build-agent-01".into(),
        os: "Windows".into(),
        os_version: "11".into(),
        arch: "x86_64".into(),
        uptime_seconds: 864_000,
        cpu: CpuInfo {
            usage_percent: 98.7,
            logical_cores: 8,
        },
        memory: MemoryInfo {
            total_bytes: 17_179_869_184,
            used_bytes: 9_000_000_000,
        },
        disks: vec![DiskInfo {
            mount: "C:".into(),
            total_bytes: 512_000_000_000,
            free_bytes: 120_000_000_000,
        }],
    }));
    let process_list = read(ReadContextOutput::ProcessList(ProcessListOutput {
        processes: vec![
            ProcessEntry {
                pid: 7321,
                name: "ffmpeg.exe".into(),
                cpu_percent: 760.0,
                memory_bytes: 1_500_000_000,
                user: Some("BUILD\\ci".into()),
                command_line_redacted: false,
            },
            ProcessEntry {
                pid: 1044,
                name: "System".into(),
                cpu_percent: 12.0,
                memory_bytes: 200_000_000,
                user: None,
                command_line_redacted: false,
            },
        ],
        truncated: false,
    }));
    EvidenceSnapshot::record(
        "high_cpu",
        "A single process (ffmpeg.exe) is consuming the CPU; system load is ~99%.",
        RECORDED_AT,
        vec![
            (Capability::SystemInfo, ok(system_info)),
            (Capability::ProcessList, ok(process_list)),
        ],
    )
}

/// A port already bound, blocking a service from starting.
pub fn scenario_port_occupied() -> EvidenceSnapshot {
    let network_ports = read(ReadContextOutput::NetworkPorts(
        desk_agent_protocol::NetworkPortsOutput {
            ports: vec![
                PortEntry {
                    protocol: "tcp".into(),
                    local_address: "0.0.0.0".into(),
                    local_port: 8080,
                    pid: Some(5120),
                    process_name: Some("old-api.exe".into()),
                },
                PortEntry {
                    protocol: "tcp".into(),
                    local_address: "127.0.0.1".into(),
                    local_port: 5432,
                    pid: Some(2210),
                    process_name: Some("postgres.exe".into()),
                },
            ],
            truncated: false,
        },
    ));
    let process_list = read(ReadContextOutput::ProcessList(ProcessListOutput {
        processes: vec![ProcessEntry {
            pid: 5120,
            name: "old-api.exe".into(),
            cpu_percent: 0.3,
            memory_bytes: 80_000_000,
            user: Some("SVC\\app".into()),
            command_line_redacted: false,
        }],
        truncated: false,
    }));
    EvidenceSnapshot::record(
        "port_occupied",
        "Port 8080 is already held by a stale process (old-api.exe, pid 5120), \
         so the new service cannot bind.",
        RECORDED_AT,
        vec![
            (Capability::NetworkPorts, ok(network_ports)),
            (Capability::ProcessList, ok(process_list)),
        ],
    )
}

/// A container that exited with an error, plus its logs and an unavailable
/// inspect (to exercise the mixed Ok/Err shape on replay).
pub fn scenario_container_failure() -> EvidenceSnapshot {
    let container_list = read(ReadContextOutput::ContainerList(
        desk_agent_protocol::ContainerListOutput {
            containers: vec![ContainerSummary {
                id: "9f3c1a2b4d5e".into(),
                name: "payments-api".into(),
                image: "registry.local/payments:1.4.2".into(),
                state: "exited".into(),
            }],
            truncated: false,
        },
    ));
    let container_logs = read(ReadContextOutput::ContainerLogs(ContainerLogsOutput {
        lines: vec![
            "2026-06-12T00:00:01Z FATAL could not connect to database: connection refused".into(),
            "2026-06-12T00:00:01Z exiting with code 1".into(),
        ],
        redactions: Vec::new(),
        truncated: false,
    }));
    let inspect_unavailable = AgentOutcome::Err(AgentError {
        kind: AgentErrorKind::InvalidInput,
        message: "no such container: payments-api-old".into(),
        retryable: false,
        safe_for_model: true,
    });
    EvidenceSnapshot::record(
        "container_failure",
        "Container payments-api exited (code 1) failing to reach its database; \
         logs captured, and a stale container id inspect returns an error.",
        RECORDED_AT,
        vec![
            (Capability::ContainerList, ok(container_list)),
            (Capability::ContainerLogs, ok(container_logs)),
            (Capability::ContainerInspect, inspect_unavailable),
        ],
    )
}

/// All three acceptance scenarios.
pub fn all_scenarios() -> Vec<EvidenceSnapshot> {
    vec![
        scenario_high_cpu(),
        scenario_port_occupied(),
        scenario_container_failure(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    // Committed fixtures, loaded at compile time so replay needs no filesystem.
    const FIXTURE_HIGH_CPU: &str = include_str!("fixtures/high_cpu.json");
    const FIXTURE_PORT_OCCUPIED: &str = include_str!("fixtures/port_occupied.json");
    const FIXTURE_CONTAINER_FAILURE: &str = include_str!("fixtures/container_failure.json");

    /// Regenerate the committed fixtures from the in-code scenario builders.
    /// Ignored by default — run explicitly after changing a builder:
    /// `cargo test -p lcxl-remote-desk-server regenerate_evidence_fixtures -- --ignored`.
    #[test]
    #[ignore = "regenerates committed fixtures; run explicitly"]
    fn regenerate_evidence_fixtures() {
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/worker/agent/eval/fixtures");
        std::fs::create_dir_all(&dir).unwrap();
        for snapshot in all_scenarios() {
            let path = dir.join(format!("{}.json", snapshot.scenario));
            std::fs::write(&path, snapshot.to_json_pretty().unwrap()).unwrap();
        }
    }

    /// Every committed fixture parses offline and matches its in-code builder,
    /// proving the snapshots are deterministic and replayable without a live
    /// machine.
    #[test]
    fn fixtures_replay_offline() {
        let cases = [
            (FIXTURE_HIGH_CPU, scenario_high_cpu()),
            (FIXTURE_PORT_OCCUPIED, scenario_port_occupied()),
            (FIXTURE_CONTAINER_FAILURE, scenario_container_failure()),
        ];
        for (json, expected) in cases {
            let loaded = EvidenceSnapshot::from_json(json).expect("fixture must parse");
            assert_eq!(loaded, expected, "fixture drifted from its builder");
        }
    }

    /// The container-failure snapshot carries a mixed Ok/Err set and survives a
    /// JSON round-trip, confirming error outcomes replay too.
    #[test]
    fn mixed_outcomes_round_trip() {
        let snap = scenario_container_failure();
        let json = snap.to_json_pretty().unwrap();
        let back = EvidenceSnapshot::from_json(&json).unwrap();
        assert_eq!(snap, back);
        let has_err = back
            .contexts
            .iter()
            .any(|c| matches!(c.outcome, AgentOutcome::Err(_)));
        assert!(has_err, "container scenario must include an error outcome");
    }

    /// Sizes are recorded per entry and the total is their sum (the M1b
    /// budget input).
    #[test]
    fn records_serialized_sizes() {
        let snap = scenario_high_cpu();
        assert!(snap.contexts.iter().all(|c| c.size_bytes > 0));
        let expected_total: usize = snap.contexts.iter().map(|c| c.size_bytes).sum();
        assert_eq!(snap.total_size_bytes(), expected_total);
    }

    /// The redactions placeholder exists and is empty in M1a.
    #[test]
    fn redactions_placeholder_is_empty() {
        for snap in all_scenarios() {
            for ctx in &snap.contexts {
                assert!(ctx.redactions.is_empty());
            }
        }
    }
}
