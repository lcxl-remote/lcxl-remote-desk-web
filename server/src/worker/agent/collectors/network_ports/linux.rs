//! Linux backend for `network.ports`: parse `/proc/net/{tcp,tcp6,udp,udp6}`.
//!
//! Best-effort — addresses and ports only, no owning PID (the inode→PID scan
//! over `/proc/*/fd` is costly and permission-limited, so it is deferred). A
//! missing family file (e.g. no IPv6) is skipped rather than failing.

use desk_agent_protocol::{AgentError, AgentErrorKind};

use super::{Protocol, RawPort, parse_proc_net_line};

pub(super) fn enumerate() -> Result<Vec<RawPort>, AgentError> {
    let sources = [
        ("/proc/net/tcp", Protocol::Tcp, false),
        ("/proc/net/tcp6", Protocol::Tcp, true),
        ("/proc/net/udp", Protocol::Udp, false),
        ("/proc/net/udp6", Protocol::Udp, true),
    ];

    let mut ports = Vec::new();
    for (path, protocol, is_v6) in sources {
        match std::fs::read_to_string(path) {
            Ok(contents) => {
                ports.extend(
                    contents
                        .lines()
                        .filter_map(|line| parse_proc_net_line(line, protocol, is_v6)),
                );
            }
            // A family without a procfs entry (IPv6 disabled, etc.) is normal.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(AgentError {
                    kind: AgentErrorKind::Internal,
                    message: format!("failed to read {path}: {e}"),
                    retryable: true,
                    safe_for_model: true,
                    error_code: None,
                });
            }
        }
    }
    Ok(ports)
}
