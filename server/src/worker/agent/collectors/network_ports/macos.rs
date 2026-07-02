//! macOS backend for `network.ports`: per-process socket enumeration via libproc.
//!
//! Iterates every PID, lists its socket file descriptors, and reads each socket's
//! info (`proc_pidfdinfo` / `PROC_PIDFDSOCKETINFO`) — the same data source as
//! `lsof`. TCP sockets are filtered to the LISTEN state; UDP sockets are all bound
//! ports. Each port carries its owning PID. A process whose info cannot be read
//! (it exited, or is not inspectable without elevation) is skipped rather than
//! failing the whole enumeration, so an unprivileged run still returns a partial
//! view instead of an error.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use desk_agent_protocol::{AgentError, AgentErrorKind};
use libproc::bsd_info::BSDInfo;
use libproc::file_info::{ListFDs, ProcFDType, pidfdinfo};
use libproc::net_info::{InSockInfo, SocketFDInfo, SocketInfoKind, TcpSIState};
use libproc::proc_pid::{listpidinfo, pidinfo};
use libproc::processes::{ProcFilter, pids_by_type};

use super::{Protocol, RawPort};

/// `insi_vflag` bit set for an IPv4 socket.
const INI_IPV4: u8 = 0x1;
/// `insi_vflag` bit set for an IPv6 socket.
const INI_IPV6: u8 = 0x2;

pub(super) fn enumerate() -> Result<Vec<RawPort>, AgentError> {
    let pids = pids_by_type(ProcFilter::All).map_err(|e| AgentError {
        kind: AgentErrorKind::Internal,
        message: format!("failed to list processes: {e}"),
        retryable: true,
        safe_for_model: true,
        error_code: None,
    })?;
    let mut ports = Vec::new();
    for pid in pids {
        // PID 0 is the kernel; it has no inspectable user sockets.
        if pid != 0 {
            collect_pid(pid as i32, &mut ports);
        }
    }
    Ok(ports)
}

/// Append every listening-TCP / bound-UDP socket owned by `pid`. Best-effort: a
/// process that vanished or is not inspectable is skipped silently — a partial
/// view beats failing the whole call.
fn collect_pid(pid: i32, out: &mut Vec<RawPort>) {
    let Ok(info) = pidinfo::<BSDInfo>(pid, 0) else {
        return;
    };
    let Ok(fds) = listpidinfo::<ListFDs>(pid, info.pbi_nfiles as usize) else {
        return;
    };
    for fd in fds {
        if !matches!(ProcFDType::from(fd.proc_fdtype), ProcFDType::Socket) {
            continue;
        }
        let Ok(socket) = pidfdinfo::<SocketFDInfo>(pid, fd.proc_fd) else {
            continue;
        };
        let si = socket.psi;
        match SocketInfoKind::from(si.soi_kind) {
            SocketInfoKind::Tcp => {
                // SAFETY: `soi_kind == Tcp` selects the `pri_tcp` union arm.
                let tcp = unsafe { si.soi_proto.pri_tcp };
                if matches!(TcpSIState::from(tcp.tcpsi_state), TcpSIState::Listen)
                    && let Some((addr, port)) = local_endpoint(&tcp.tcpsi_ini)
                {
                    out.push(RawPort {
                        protocol: Protocol::Tcp,
                        local_addr: addr,
                        local_port: port,
                        pid: Some(pid as u32),
                    });
                }
            }
            SocketInfoKind::In if si.soi_protocol == libc::IPPROTO_UDP => {
                // SAFETY: `soi_kind == In` selects the `pri_in` union arm.
                let ini = unsafe { si.soi_proto.pri_in };
                if let Some((addr, port)) = local_endpoint(&ini) {
                    out.push(RawPort {
                        protocol: Protocol::Udp,
                        local_addr: addr,
                        local_port: port,
                        pid: Some(pid as u32),
                    });
                }
            }
            _ => {}
        }
    }
}

/// Decode the local address + port from an `InSockInfo`. The port is stored in
/// network byte order; the IPv4 address is an in-memory network-order word and the
/// IPv6 address a 16-byte network-order array. Returns `None` for a socket with no
/// address family flagged (e.g. an unbound socket).
fn local_endpoint(ini: &InSockInfo) -> Option<(IpAddr, u16)> {
    let port = u16::from_be(ini.insi_lport as u16);
    if ini.insi_vflag & INI_IPV4 != 0 {
        // SAFETY: the IPv4 flag selects the `ina_46` (v4-in-v6) union arm.
        let s_addr = unsafe { ini.insi_laddr.ina_46.i46a_addr4.s_addr };
        let octets = s_addr.to_ne_bytes();
        Some((
            IpAddr::V4(Ipv4Addr::new(octets[0], octets[1], octets[2], octets[3])),
            port,
        ))
    } else if ini.insi_vflag & INI_IPV6 != 0 {
        // SAFETY: the IPv6 flag selects the `ina_6` union arm.
        let addr = unsafe { ini.insi_laddr.ina_6 };
        Some((IpAddr::V6(Ipv6Addr::from(addr.s6_addr)), port))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// Bind a real listening TCP socket and assert the backend surfaces it with the
    /// correct port (proving the network-byte-order decode), owning PID, and LISTEN
    /// filtering. Exercises the live libproc path end to end.
    #[test]
    fn finds_a_bound_listening_tcp_port() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let want_port = listener.local_addr().unwrap().port();
        let me = std::process::id();

        let ports = enumerate().expect("enumeration succeeds");
        let found = ports.iter().find(|p| {
            p.protocol == Protocol::Tcp && p.local_port == want_port && p.pid == Some(me)
        });
        assert!(
            found.is_some(),
            "expected to find our listening port {want_port} owned by pid {me}"
        );
    }
}
