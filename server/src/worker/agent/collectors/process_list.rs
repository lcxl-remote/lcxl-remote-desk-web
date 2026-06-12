//! `process.list` collector — a sorted, capped process snapshot.
//!
//! Backed by `sysinfo` (no platform branch). Per-process CPU percentages need
//! two samples spaced by [`sysinfo::MINIMUM_CPU_UPDATE_INTERVAL`]; the
//! collector takes that short sleep on the blocking pool (it runs under
//! `spawn_blocking`).

use desk_agent_protocol::{ProcessEntry, ProcessListOutput, ProcessListParams, ProcessSort};
use sysinfo::{ProcessesToUpdate, System, Users};

/// Cap applied when the caller leaves `limit` at its default (0). Keeps a full
/// process table from flooding a model's context window; a caller that wants
/// more sets an explicit `limit`.
const DEFAULT_LIMIT: usize = 100;

/// Collect the process table, sorted per `params.sort` and truncated to the
/// effective limit. Infallible: `sysinfo` yields best-effort data, and a host
/// with no enumerable processes simply returns an empty list.
pub fn collect(params: &ProcessListParams) -> ProcessListOutput {
    let mut sys = System::new_all();
    // The first CPU sample came from `new_all`; a second after the minimum
    // interval is required for a meaningful per-process percentage.
    std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
    sys.refresh_processes(ProcessesToUpdate::All, true);

    // Resolve owner uids to human usernames; far more useful to a model than a
    // raw uid / SID, and portable across platforms.
    let users = Users::new_with_refreshed_list();

    let mut entries: Vec<ProcessEntry> = sys
        .processes()
        .values()
        .map(|proc| {
            let user = proc
                .user_id()
                .and_then(|uid| users.get_user_by_id(uid))
                .map(|u| u.name().to_string());
            // M1a never emits raw command lines: they require the redaction
            // pipeline that lands in M1b. When the caller asks for them and a
            // command line exists, flag it redacted so the model knows the
            // data was withheld rather than absent.
            let command_line_redacted = params.include_command_line && !proc.cmd().is_empty();
            ProcessEntry {
                pid: proc.pid().as_u32(),
                name: proc.name().to_string_lossy().into_owned(),
                cpu_percent: proc.cpu_usage(),
                memory_bytes: proc.memory(),
                user,
                command_line_redacted,
            }
        })
        .collect();

    match params.sort {
        // `total_cmp` gives a total order over f32 (handles NaN deterministically).
        ProcessSort::CpuDesc => entries.sort_by(|a, b| b.cpu_percent.total_cmp(&a.cpu_percent)),
        ProcessSort::MemoryDesc => entries.sort_by(|a, b| b.memory_bytes.cmp(&a.memory_bytes)),
        ProcessSort::Pid => entries.sort_by_key(|e| e.pid),
    }

    let limit = if params.limit == 0 {
        DEFAULT_LIMIT
    } else {
        params.limit as usize
    };
    let truncated = entries.len() > limit;
    entries.truncate(limit);

    ProcessListOutput {
        processes: entries,
        truncated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(limit: u32, sort: ProcessSort, include_command_line: bool) -> ProcessListParams {
        ProcessListParams {
            limit,
            sort,
            include_command_line,
        }
    }

    #[test]
    fn collect_returns_processes_with_pids() {
        // The test process itself is always present, so the table is non-empty
        // and every entry carries a non-zero pid.
        let out = collect(&params(0, ProcessSort::CpuDesc, false));
        assert!(!out.processes.is_empty());
        assert!(out.processes.iter().all(|p| p.pid > 0));
    }

    #[test]
    fn cpu_desc_sort_is_monotonic() {
        let out = collect(&params(0, ProcessSort::CpuDesc, false));
        for win in out.processes.windows(2) {
            assert!(win[0].cpu_percent >= win[1].cpu_percent);
        }
    }

    #[test]
    fn memory_desc_sort_is_monotonic() {
        let out = collect(&params(0, ProcessSort::MemoryDesc, false));
        for win in out.processes.windows(2) {
            assert!(win[0].memory_bytes >= win[1].memory_bytes);
        }
    }

    #[test]
    fn pid_sort_is_ascending() {
        let out = collect(&params(0, ProcessSort::Pid, false));
        for win in out.processes.windows(2) {
            assert!(win[0].pid <= win[1].pid);
        }
    }

    #[test]
    fn explicit_limit_truncates_and_flags() {
        let full = collect(&params(0, ProcessSort::Pid, false));
        // Only meaningful when the host has more than one process (it always does).
        let out = collect(&params(1, ProcessSort::Pid, false));
        assert_eq!(out.processes.len(), 1);
        if full.processes.len() > 1 {
            assert!(out.truncated);
        }
    }

    #[test]
    fn command_line_is_redacted_when_requested() {
        // With command lines requested, at least one real process (e.g. the
        // test runner) has a non-empty cmd and must be flagged redacted; M1a
        // never emits the raw line itself.
        let out = collect(&params(0, ProcessSort::CpuDesc, true));
        assert!(out.processes.iter().any(|p| p.command_line_redacted));

        // Without the request, nothing is flagged redacted.
        let plain = collect(&params(0, ProcessSort::CpuDesc, false));
        assert!(plain.processes.iter().all(|p| !p.command_line_redacted));
    }
}
