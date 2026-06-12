//! `system.info` collector — a point-in-time host snapshot.
//!
//! Backed by `sysinfo`, which abstracts the per-OS source internally, so this
//! collector carries no platform branch. CPU usage requires two samples
//! spaced by [`sysinfo::MINIMUM_CPU_UPDATE_INTERVAL`]; the collector takes
//! that short sleep on the blocking pool (it runs under `spawn_blocking`).

use desk_agent_protocol::{CpuInfo, DiskInfo, MemoryInfo, SystemInfoOutput, SystemInfoParams};
use sysinfo::{Disks, System};

/// Collect the host snapshot. Infallible: `sysinfo` returns best-effort data
/// (empty/zero fields rather than errors) on platforms or under permissions
/// where a source is unavailable, so the model always gets a well-formed
/// output. `params` flags are accepted for forward compatibility; the base
/// snapshot is always populated and the optional sections are inexpensive
/// enough to always include for now.
pub fn collect(_params: &SystemInfoParams) -> SystemInfoOutput {
    let mut sys = System::new_all();
    // `new_all` primes the CPU counters; a second refresh after the minimum
    // interval is required for a meaningful per-core / global usage delta.
    std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
    sys.refresh_cpu_all();

    let disks = Disks::new_with_refreshed_list()
        .iter()
        .map(|disk| DiskInfo {
            mount: disk.mount_point().to_string_lossy().into_owned(),
            total_bytes: disk.total_space(),
            free_bytes: disk.available_space(),
        })
        .collect();

    SystemInfoOutput {
        hostname: System::host_name().unwrap_or_default(),
        os: System::name().unwrap_or_default(),
        os_version: System::os_version().unwrap_or_default(),
        arch: System::cpu_arch(),
        uptime_seconds: System::uptime(),
        cpu: CpuInfo {
            usage_percent: sys.global_cpu_usage(),
            logical_cores: sys.cpus().len() as u32,
        },
        memory: MemoryInfo {
            total_bytes: sys.total_memory(),
            used_bytes: sys.used_memory(),
        },
        disks,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_populates_core_fields() {
        let out = collect(&SystemInfoParams::default());
        // Logical core count and total memory are reliably non-zero on any
        // host that can run the test suite.
        assert!(out.cpu.logical_cores >= 1);
        assert!(out.memory.total_bytes > 0);
        // Used memory can never exceed total.
        assert!(out.memory.used_bytes <= out.memory.total_bytes);
        // Arch is always reported by sysinfo (e.g. "x86_64", "aarch64").
        assert!(!out.arch.is_empty());
    }

    #[test]
    fn disks_report_free_within_total() {
        let out = collect(&SystemInfoParams::default());
        for disk in &out.disks {
            assert!(disk.free_bytes <= disk.total_bytes);
        }
    }
}
