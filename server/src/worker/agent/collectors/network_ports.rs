//! `network.ports` collector — listening/bound local ports with owning PID.
//!
//! Platform-dispatched:
//! - **Windows** enumerates via the IpHelper extended tables
//!   (`GetExtendedTcpTable` / `GetExtendedUdpTable`) with full owning-PID
//!   resolution (TCP is filtered to listening sockets; UDP is all bound ports).
//! - **Linux** parses `/proc/net/{tcp,tcp6,udp,udp6}` best-effort (addresses +
//!   ports, no PID — the inode→PID scan is costly and permission-limited, so it
//!   is deferred). TCP is filtered to the LISTEN state.
//! - Other platforms return `UnsupportedPlatform`.
//!
//! PIDs (when present) are resolved to process names via `sysinfo`.

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;

use desk_agent_protocol::{
    AgentError, AgentErrorKind, NetworkPortsOutput, NetworkPortsParams, PortEntry,
};
use sysinfo::{Pid, ProcessesToUpdate, System};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod windows;

/// Hard cap on returned ports; a host with more sets `truncated`. Listening
/// sockets rarely approach this, so it is a safety bound, not a tuning knob.
const MAX_PORTS: usize = 500;

/// A raw enumerated socket, before PID→name resolution. Produced by the
/// platform backend, consumed by [`collect`].
pub(crate) struct RawPort {
    pub protocol: Protocol,
    pub local_addr: IpAddr,
    pub local_port: u16,
    pub pid: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Protocol {
    Tcp,
    Udp,
}

impl Protocol {
    fn as_str(self) -> &'static str {
        match self {
            Protocol::Tcp => "tcp",
            Protocol::Udp => "udp",
        }
    }
}

/// Collect listening/bound local ports. Returns `UnsupportedPlatform` on a
/// platform without a backend, `InvalidInput` for an unknown protocol filter.
pub fn collect(params: &NetworkPortsParams) -> Result<NetworkPortsOutput, AgentError> {
    let filter = match params.protocol.as_deref() {
        None => None,
        Some(p) if p.eq_ignore_ascii_case("tcp") => Some(Protocol::Tcp),
        Some(p) if p.eq_ignore_ascii_case("udp") => Some(Protocol::Udp),
        Some(other) => {
            return Err(AgentError {
                kind: AgentErrorKind::InvalidInput,
                message: format!("unknown protocol filter {other:?}; expected \"tcp\" or \"udp\""),
                retryable: false,
                safe_for_model: true,
            });
        }
    };

    let raw = enumerate()?;
    let names = resolve_names(&raw);

    let mut ports: Vec<PortEntry> = raw
        .into_iter()
        .filter(|p| match filter {
            None => true,
            Some(f) => f == p.protocol,
        })
        .map(|p| PortEntry {
            protocol: p.protocol.as_str().to_string(),
            local_address: p.local_addr.to_string(),
            local_port: p.local_port,
            process_name: p.pid.and_then(|pid| names.get(&pid).cloned()),
            pid: p.pid,
        })
        .collect();

    // Stable, model-friendly ordering: by port, then protocol.
    ports.sort_by(|a, b| {
        a.local_port
            .cmp(&b.local_port)
            .then_with(|| a.protocol.cmp(&b.protocol))
    });

    let truncated = ports.len() > MAX_PORTS;
    ports.truncate(MAX_PORTS);

    Ok(NetworkPortsOutput { ports, truncated })
}

/// Resolve the owning PIDs present in `raw` to process names in one `sysinfo`
/// pass. Returns an empty map when no port carries a PID (e.g. Linux).
fn resolve_names(raw: &[RawPort]) -> HashMap<u32, String> {
    let want: HashSet<u32> = raw.iter().filter_map(|p| p.pid).collect();
    if want.is_empty() {
        return HashMap::new();
    }
    let mut sys = System::new();
    sys.refresh_processes(ProcessesToUpdate::All, false);
    want.into_iter()
        .filter_map(|pid| {
            sys.process(Pid::from_u32(pid))
                .map(|proc| (pid, proc.name().to_string_lossy().into_owned()))
        })
        .collect()
}

fn enumerate() -> Result<Vec<RawPort>, AgentError> {
    #[cfg(windows)]
    {
        windows::enumerate()
    }
    #[cfg(target_os = "linux")]
    {
        linux::enumerate()
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        Err(AgentError {
            kind: AgentErrorKind::UnsupportedPlatform,
            message: "network.ports is not supported on this platform".to_string(),
            retryable: false,
            safe_for_model: true,
        })
    }
}

/// Parse a single `/proc/net/{tcp,tcp6,udp,udp6}` data line into a [`RawPort`].
/// Returns `None` for the header row, malformed lines, and (for TCP)
/// non-listening sockets. Kept platform-agnostic so the hex decoding is unit
/// tested on any host; the Linux backend supplies the file contents.
#[cfg(any(target_os = "linux", test))]
fn parse_proc_net_line(line: &str, protocol: Protocol, is_v6: bool) -> Option<RawPort> {
    /// TCP state code for LISTEN in `/proc/net/tcp` (hex `0A`).
    const TCP_LISTEN: &str = "0A";

    let mut fields = line.split_whitespace();
    // First column is the slot index "N:"; the header row's first token is
    // "sl" (no trailing colon), so this rejects it.
    let slot = fields.next()?;
    if !slot.ends_with(':') {
        return None;
    }
    let local = fields.next()?; // local_address "HEXIP:HEXPORT"
    let _remote = fields.next()?;
    let state = fields.next()?;
    if protocol == Protocol::Tcp && !state.eq_ignore_ascii_case(TCP_LISTEN) {
        return None;
    }
    let (local_addr, local_port) = parse_hex_addr_port(local, is_v6)?;
    Some(RawPort {
        protocol,
        local_addr,
        local_port,
        pid: None,
    })
}

/// Decode a `/proc/net` `HEXIP:HEXPORT` token. The IPv4 address is a single
/// little-endian 32-bit word; the IPv6 address is four little-endian 32-bit
/// words (16 bytes). The port is plain big-endian hex.
#[cfg(any(target_os = "linux", test))]
fn parse_hex_addr_port(token: &str, is_v6: bool) -> Option<(IpAddr, u16)> {
    use std::net::{Ipv4Addr, Ipv6Addr};

    let (ip_hex, port_hex) = token.split_once(':')?;
    let port = u16::from_str_radix(port_hex, 16).ok()?;
    let addr = if is_v6 {
        if ip_hex.len() != 32 {
            return None;
        }
        let mut bytes = [0u8; 16];
        for word in 0..4 {
            let raw = u32::from_str_radix(&ip_hex[word * 8..word * 8 + 8], 16).ok()?;
            bytes[word * 4..word * 4 + 4].copy_from_slice(&raw.to_le_bytes());
        }
        IpAddr::V6(Ipv6Addr::from(bytes))
    } else {
        if ip_hex.len() != 8 {
            return None;
        }
        let raw = u32::from_str_radix(ip_hex, 16).ok()?;
        IpAddr::V4(Ipv4Addr::from(raw.to_le_bytes()))
    };
    Some((addr, port))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn parses_ipv4_loopback_listen_line() {
        // A real `/proc/net/tcp` LISTEN row for 127.0.0.1:631 (0x0277).
        let line = "   0: 0100007F:0277 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 12345 1 0000000000000000 100 0 0 10 0";
        let port = parse_proc_net_line(line, Protocol::Tcp, false).expect("should parse");
        assert_eq!(port.protocol, Protocol::Tcp);
        assert_eq!(port.local_addr, IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)));
        assert_eq!(port.local_port, 0x0277);
        assert_eq!(port.pid, None);
    }

    #[test]
    fn skips_header_row() {
        let header = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode";
        assert!(parse_proc_net_line(header, Protocol::Tcp, false).is_none());
    }

    #[test]
    fn skips_non_listening_tcp() {
        // st = 01 (ESTABLISHED) must be dropped for TCP.
        let line = "   1: 0100007F:0050 0100007F:1234 01 00000000:00000000 00:00000000 00000000  1000        0 1 1 0";
        assert!(parse_proc_net_line(line, Protocol::Tcp, false).is_none());
    }

    #[test]
    fn keeps_all_udp_states() {
        // UDP has no LISTEN filter; an arbitrary state column is still kept.
        let line = "   2: 00000000:14E9 00000000:0000 07 00000000:00000000 00:00000000 00000000  1000        0 1 2 0";
        let port = parse_proc_net_line(line, Protocol::Udp, false).expect("udp should parse");
        assert_eq!(port.local_port, 0x14E9);
        assert_eq!(port.local_addr, IpAddr::V4(Ipv4Addr::UNSPECIFIED));
    }

    #[test]
    fn parses_ipv6_loopback() {
        // IPv6 ::1 is 31 zero nibbles then 1, stored as four LE words.
        let token = "00000000000000000000000001000000:1F90";
        let (addr, port) = parse_hex_addr_port(token, true).expect("v6 parse");
        assert_eq!(addr, IpAddr::V6(std::net::Ipv6Addr::LOCALHOST));
        assert_eq!(port, 0x1F90);
    }

    #[test]
    fn rejects_wrong_width_addresses() {
        assert!(parse_hex_addr_port("DEAD:1F90", false).is_none());
        assert!(parse_hex_addr_port("0100007F:1F90", true).is_none());
    }

    /// Live enumeration on a platform with a backend: results must be
    /// well-formed and the protocol filter must narrow them. Exercises the
    /// Windows IpHelper FFI / Linux procfs path end to end.
    #[cfg(any(windows, target_os = "linux"))]
    #[test]
    fn live_enumeration_is_well_formed() {
        let out = collect(&NetworkPortsParams::default()).expect("supported platform");
        assert!(out.ports.len() <= MAX_PORTS);
        for entry in &out.ports {
            assert!(entry.protocol == "tcp" || entry.protocol == "udp");
            assert!(!entry.local_address.is_empty());
        }

        let tcp_only = collect(&NetworkPortsParams {
            protocol: Some("TCP".to_string()),
        })
        .expect("supported platform");
        assert!(tcp_only.ports.iter().all(|entry| entry.protocol == "tcp"));
    }

    #[test]
    fn unknown_protocol_filter_is_invalid_input() {
        let params = NetworkPortsParams {
            protocol: Some("sctp".to_string()),
        };
        let err = collect(&params).expect_err("unknown protocol must reject");
        assert_eq!(err.kind, AgentErrorKind::InvalidInput);
    }
}
