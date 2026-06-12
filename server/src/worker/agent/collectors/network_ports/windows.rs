//! Windows backend for `network.ports`: enumerate via the IpHelper extended
//! tables with owning-PID resolution.
//!
//! TCP is filtered to listening sockets (the diagnostic intent of "what is
//! occupying a port"); UDP returns every bound socket (UDP has no listen
//! state). Each table is fetched with the documented two-call sizing dance:
//! a first call with a null buffer reports the required size, the second
//! fills it.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use desk_agent_protocol::{AgentError, AgentErrorKind};
use windows::Win32::NetworkManagement::IpHelper::{
    GetExtendedTcpTable, GetExtendedUdpTable, MIB_TCP_STATE_LISTEN, MIB_TCP6TABLE_OWNER_PID,
    MIB_TCPTABLE_OWNER_PID, MIB_UDP6TABLE_OWNER_PID, MIB_UDPTABLE_OWNER_PID,
    TCP_TABLE_OWNER_PID_ALL, UDP_TABLE_OWNER_PID,
};

use super::{Protocol, RawPort};

/// `AF_INET` / `AF_INET6` address families (from WinSock); declared locally to
/// avoid pulling the whole WinSock feature in for two constants.
const AF_INET: u32 = 2;
const AF_INET6: u32 = 23;

pub(super) fn enumerate() -> Result<Vec<RawPort>, AgentError> {
    let mut ports = Vec::new();
    tcp4(&mut ports)?;
    tcp6(&mut ports)?;
    udp4(&mut ports)?;
    udp6(&mut ports)?;
    Ok(ports)
}

/// Convert a `dw*Port` field (port number in network byte order, in the low
/// 16 bits) to host order.
fn ntohs(dw_port: u32) -> u16 {
    u16::from_be_bytes([(dw_port & 0xff) as u8, ((dw_port >> 8) & 0xff) as u8])
}

fn table_query_err(code: u32) -> AgentError {
    AgentError {
        kind: AgentErrorKind::Internal,
        message: format!("IpHelper table query failed (code {code})"),
        retryable: true,
        safe_for_model: true,
    }
}

/// Run the two-call sizing dance and return the filled table buffer as a
/// `Vec<u32>` (guarantees 4-byte alignment for the `repr(C)` table cast). An
/// empty table yields an empty vec.
fn fetch_table(
    mut call: impl FnMut(Option<*mut core::ffi::c_void>, *mut u32) -> u32,
) -> Result<Vec<u32>, AgentError> {
    let mut size: u32 = 0;
    // Sizing call: expected to fail with ERROR_INSUFFICIENT_BUFFER and set size.
    let _ = call(None, &mut size);
    if size == 0 {
        return Ok(Vec::new());
    }
    let mut buffer = vec![0u32; (size as usize).div_ceil(4)];
    let ret = call(Some(buffer.as_mut_ptr().cast()), &mut size);
    if ret != 0 {
        return Err(table_query_err(ret));
    }
    Ok(buffer)
}

fn tcp4(out: &mut Vec<RawPort>) -> Result<(), AgentError> {
    let buffer = fetch_table(|ptr, size| unsafe {
        GetExtendedTcpTable(ptr, size, false, AF_INET, TCP_TABLE_OWNER_PID_ALL, 0)
    })?;
    if buffer.is_empty() {
        return Ok(());
    }
    // SAFETY: `buffer` was filled by GetExtendedTcpTable for AF_INET with the
    // OWNER_PID_ALL class, so it is a valid `MIB_TCPTABLE_OWNER_PID` whose
    // `dwNumEntries` bounds the trailing row array; the buffer is u32-aligned.
    unsafe {
        let table = &*(buffer.as_ptr() as *const MIB_TCPTABLE_OWNER_PID);
        let rows = std::slice::from_raw_parts(table.table.as_ptr(), table.dwNumEntries as usize);
        for row in rows {
            if row.dwState != MIB_TCP_STATE_LISTEN.0 as u32 {
                continue;
            }
            out.push(RawPort {
                protocol: Protocol::Tcp,
                local_addr: IpAddr::V4(Ipv4Addr::from(row.dwLocalAddr.to_ne_bytes())),
                local_port: ntohs(row.dwLocalPort),
                pid: Some(row.dwOwningPid),
            });
        }
    }
    Ok(())
}

fn tcp6(out: &mut Vec<RawPort>) -> Result<(), AgentError> {
    let buffer = fetch_table(|ptr, size| unsafe {
        GetExtendedTcpTable(ptr, size, false, AF_INET6, TCP_TABLE_OWNER_PID_ALL, 0)
    })?;
    if buffer.is_empty() {
        return Ok(());
    }
    // SAFETY: as `tcp4`, for the AF_INET6 `MIB_TCP6TABLE_OWNER_PID` layout.
    unsafe {
        let table = &*(buffer.as_ptr() as *const MIB_TCP6TABLE_OWNER_PID);
        let rows = std::slice::from_raw_parts(table.table.as_ptr(), table.dwNumEntries as usize);
        for row in rows {
            if row.dwState != MIB_TCP_STATE_LISTEN.0 as u32 {
                continue;
            }
            out.push(RawPort {
                protocol: Protocol::Tcp,
                local_addr: IpAddr::V6(Ipv6Addr::from(row.ucLocalAddr)),
                local_port: ntohs(row.dwLocalPort),
                pid: Some(row.dwOwningPid),
            });
        }
    }
    Ok(())
}

fn udp4(out: &mut Vec<RawPort>) -> Result<(), AgentError> {
    let buffer = fetch_table(|ptr, size| unsafe {
        GetExtendedUdpTable(ptr, size, false, AF_INET, UDP_TABLE_OWNER_PID, 0)
    })?;
    if buffer.is_empty() {
        return Ok(());
    }
    // SAFETY: as `tcp4`, for the AF_INET `MIB_UDPTABLE_OWNER_PID` layout.
    unsafe {
        let table = &*(buffer.as_ptr() as *const MIB_UDPTABLE_OWNER_PID);
        let rows = std::slice::from_raw_parts(table.table.as_ptr(), table.dwNumEntries as usize);
        for row in rows {
            out.push(RawPort {
                protocol: Protocol::Udp,
                local_addr: IpAddr::V4(Ipv4Addr::from(row.dwLocalAddr.to_ne_bytes())),
                local_port: ntohs(row.dwLocalPort),
                pid: Some(row.dwOwningPid),
            });
        }
    }
    Ok(())
}

fn udp6(out: &mut Vec<RawPort>) -> Result<(), AgentError> {
    let buffer = fetch_table(|ptr, size| unsafe {
        GetExtendedUdpTable(ptr, size, false, AF_INET6, UDP_TABLE_OWNER_PID, 0)
    })?;
    if buffer.is_empty() {
        return Ok(());
    }
    // SAFETY: as `tcp4`, for the AF_INET6 `MIB_UDP6TABLE_OWNER_PID` layout.
    unsafe {
        let table = &*(buffer.as_ptr() as *const MIB_UDP6TABLE_OWNER_PID);
        let rows = std::slice::from_raw_parts(table.table.as_ptr(), table.dwNumEntries as usize);
        for row in rows {
            out.push(RawPort {
                protocol: Protocol::Udp,
                local_addr: IpAddr::V6(Ipv6Addr::from(row.ucLocalAddr)),
                local_port: ntohs(row.dwLocalPort),
                pid: Some(row.dwOwningPid),
            });
        }
    }
    Ok(())
}
