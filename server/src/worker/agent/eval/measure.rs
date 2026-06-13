//! Full-collection size / latency measurement.
//!
//! Runs each host-available read collector once, timing it and measuring the
//! serialized JSON size of its outcome. The numbers calibrate M1b's context
//! budget and latency thresholds (roadmap §M1a). This is a measurement harness,
//! not an assertion suite: the [`measure_full_collection`] test is `#[ignore]`
//! and prints a table when run with `--nocapture`; the captured figures are
//! written up in the `agent_works` measurement note.

use std::time::Instant;

use desk_agent_protocol::{AgentOutcome, OperationOutput, ReadContextOutput};

use crate::worker::agent::collectors;

/// One collector's measured cost.
#[derive(Debug, Clone)]
pub struct Measurement {
    pub capability: &'static str,
    /// Wall-clock collection time in milliseconds.
    pub latency_ms: f64,
    /// Serialized JSON size of the `AgentOutcome`, in bytes.
    pub size_bytes: usize,
    /// `ok` if the collector returned data, else the error kind / reason.
    pub status: String,
}

fn measure(
    capability: &'static str,
    collect: impl FnOnce() -> Result<ReadContextOutput, String>,
) -> Measurement {
    let started = Instant::now();
    let result = collect();
    let latency_ms = started.elapsed().as_secs_f64() * 1000.0;
    match result {
        Ok(output) => {
            let outcome = AgentOutcome::Ok(OperationOutput::ReadContext(output));
            let size_bytes = serde_json::to_vec(&outcome).map(|v| v.len()).unwrap_or(0);
            Measurement {
                capability,
                latency_ms,
                size_bytes,
                status: "ok".to_string(),
            }
        }
        Err(reason) => Measurement {
            capability,
            latency_ms,
            size_bytes: 0,
            status: reason,
        },
    }
}

/// Run every host-available read collector once and measure it. Async
/// (container) collectors are measured too when Docker is reachable, degrading
/// to an `unsupported` row otherwise.
pub async fn measure_full_collection() -> Vec<Measurement> {
    use desk_agent_protocol::{
        ContainerListParams, LogRecentParams, NetworkPortsParams, ProcessListParams,
        ServiceStatusParams, SystemInfoParams,
    };

    let mut rows = Vec::new();

    rows.push(measure("system.info", || {
        Ok(ReadContextOutput::SystemInfo(
            collectors::system_info::collect(&SystemInfoParams::default()),
        ))
    }));
    rows.push(measure("process.list", || {
        Ok(ReadContextOutput::ProcessList(
            collectors::process_list::collect(&ProcessListParams::default()),
        ))
    }));
    rows.push(measure("network.ports", || {
        collectors::network_ports::collect(&NetworkPortsParams::default())
            .map(ReadContextOutput::NetworkPorts)
            .map_err(|e| format!("{:?}", e.kind))
    }));
    rows.push(measure("service.status", || {
        collectors::service_status::collect(&ServiceStatusParams::default())
            .map(ReadContextOutput::ServiceStatus)
            .map_err(|e| format!("{:?}", e.kind))
    }));
    rows.push(measure("log.recent", || {
        collectors::log_recent::collect(&LogRecentParams::default())
            .map(ReadContextOutput::LogRecent)
            .map_err(|e| format!("{:?}", e.kind))
    }));

    // Container list is async; measure it separately.
    let started = Instant::now();
    let container = collectors::container::list(&ContainerListParams::default()).await;
    let latency_ms = started.elapsed().as_secs_f64() * 1000.0;
    rows.push(match container {
        Ok(output) => {
            let outcome = AgentOutcome::Ok(OperationOutput::ReadContext(
                ReadContextOutput::ContainerList(output),
            ));
            Measurement {
                capability: "container.list",
                latency_ms,
                size_bytes: serde_json::to_vec(&outcome).map(|v| v.len()).unwrap_or(0),
                status: "ok".to_string(),
            }
        }
        Err(e) => Measurement {
            capability: "container.list",
            latency_ms,
            size_bytes: 0,
            status: format!("{:?}", e.kind),
        },
    });

    rows
}

/// Total serialized size of all `ok` collectors, in bytes.
pub fn total_size_bytes(rows: &[Measurement]) -> usize {
    rows.iter().map(|r| r.size_bytes).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Measurement harness. Ignored by default (it is a calibration run, not an
    /// assertion). Run with:
    /// `cargo test -p lcxl-remote-desk-server measure_full_collection -- --ignored --nocapture`
    #[tokio::test]
    #[ignore = "measurement harness; run explicitly with --nocapture"]
    async fn measure_full_collection_prints_table() {
        let rows = measure_full_collection().await;
        println!(
            "\n{:<18} {:>12} {:>12}  status",
            "capability", "latency_ms", "size_bytes"
        );
        for r in &rows {
            println!(
                "{:<18} {:>12.2} {:>12}  {}",
                r.capability, r.latency_ms, r.size_bytes, r.status
            );
        }
        let total = total_size_bytes(&rows);
        let total_latency: f64 = rows.iter().map(|r| r.latency_ms).sum();
        println!(
            "{:<18} {:>12.2} {:>12}  (ok collectors)",
            "TOTAL", total_latency, total
        );
        assert!(!rows.is_empty());
    }

    /// Cheap smoke test (always runs): the always-available collectors
    /// (system.info, process.list) produce non-empty measured sizes.
    #[tokio::test]
    async fn cross_platform_collectors_are_measurable() {
        let rows = measure_full_collection().await;
        let system = rows
            .iter()
            .find(|r| r.capability == "system.info")
            .expect("system.info row");
        assert_eq!(system.status, "ok");
        assert!(system.size_bytes > 0);
    }
}
