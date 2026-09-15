//! Bind local backup access to the OS process owning the accepted TCP connection.
//! HTTP headers, forwarded addresses and requested SIDs are not identity sources.
use actix_web::{HttpRequest, dev::Extensions, rt::net::TcpStream};
use std::{
    any::Any,
    io,
    mem::{offset_of, size_of},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
};
use windows::Win32::{
    Foundation::{ERROR_INSUFFICIENT_BUFFER, FILETIME, HANDLE, WAIT_TIMEOUT},
    NetworkManagement::IpHelper::{
        GetExtendedTcpTable, MIB_TCP_STATE_ESTAB, MIB_TCP6ROW_OWNER_PID, MIB_TCP6TABLE_OWNER_PID,
        MIB_TCPROW_OWNER_PID, MIB_TCPTABLE_OWNER_PID, TCP_TABLE_OWNER_PID_ALL,
    },
    System::{
        RemoteDesktop::ProcessIdToSessionId,
        SystemInformation::GetSystemTimeAsFileTime,
        Threading::{
            GetProcessId, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
            PROCESS_SYNCHRONIZE, WaitForSingleObject,
        },
    },
};

struct Connection {
    peer: SocketAddr,
    local: SocketAddr,
    accepted_at: u64,
}

pub(crate) struct LocalUser {
    pub(crate) sid: String,
    pub(crate) session_id: u32,
    process: OwnedHandle,
}

impl LocalUser {
    pub(crate) fn is_alive(&self) -> bool {
        let handle = HANDLE(self.process.as_raw_handle());
        if unsafe { WaitForSingleObject(handle, 0) } != WAIT_TIMEOUT {
            return false;
        }
        let pid = unsafe { GetProcessId(handle) };
        let mut session = 0;
        pid != 0
            && unsafe { ProcessIdToSessionId(pid, &mut session) }.is_ok()
            && session == self.session_id
            && session != 0
            && unsafe { WaitForSingleObject(handle, 0) } == WAIT_TIMEOUT
    }
}

fn ticks(time: FILETIME) -> u64 {
    (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)
}

fn normalize(address: SocketAddr) -> SocketAddr {
    match address {
        SocketAddr::V6(v6) => v6
            .ip()
            .to_ipv4_mapped()
            .map(|ip| SocketAddr::new(IpAddr::V4(ip), v6.port()))
            .unwrap_or(address),
        _ => address,
    }
}

/// Called by HttpServer before requests are parsed. Unsupported transports have
/// no local identity, rather than falling back to a claimed Host or peer header.
pub(crate) fn on_connect(stream: &dyn Any, extensions: &mut Extensions) {
    let Some(stream) = stream.downcast_ref::<TcpStream>() else {
        return;
    };
    let (Ok(peer), Ok(local)) = (stream.peer_addr(), stream.local_addr()) else {
        return;
    };
    let (peer, local) = (normalize(peer), normalize(local));
    if peer.ip().is_loopback() && local.ip().is_loopback() {
        extensions.insert(Connection {
            peer,
            local,
            accepted_at: ticks(unsafe { GetSystemTimeAsFileTime() }),
        });
    }
}

fn denied() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "Local OS user identity is unavailable",
    )
}

pub(crate) fn authenticate(request: &HttpRequest) -> io::Result<LocalUser> {
    let connection = request.conn_data::<Connection>().ok_or_else(denied)?;
    if request.peer_addr().map(normalize) != Some(connection.peer) {
        return Err(denied());
    }
    let pid = owner_pid(connection)?;
    let handle = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            false,
            pid,
        )
    }
    .map_err(|_| denied())?;
    // OwnedHandle closes exactly this process handle; no process is terminated.
    let process = unsafe { OwnedHandle::from_raw_handle(handle.0) };
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) }
        .map_err(|_| denied())?;
    if ticks(creation) == 0 || ticks(creation) > connection.accepted_at {
        return Err(denied());
    }
    let mut session_id = 0;
    unsafe { ProcessIdToSessionId(pid, &mut session_id) }.map_err(|_| denied())?;
    if session_id == 0 {
        return Err(denied());
    }
    let sid = desk_file_recovery::windows::process_user_sid(handle).map_err(|_| denied())?;
    if matches!(sid.as_str(), "S-1-5-18" | "S-1-5-19" | "S-1-5-20") {
        return Err(denied());
    }
    let identity = LocalUser {
        sid,
        session_id,
        process,
    };
    // Pin the original process object, then re-read the exact tuple. PID reuse,
    // closed sockets and changed owners fail closed before any vault is opened.
    if !identity.is_alive() || owner_pid(connection)? != pid || !identity.is_alive() {
        return Err(denied());
    }
    Ok(identity)
}

fn table(family: u32) -> io::Result<Vec<u32>> {
    let mut size = 0;
    let code =
        unsafe { GetExtendedTcpTable(None, &mut size, false, family, TCP_TABLE_OWNER_PID_ALL, 0) };
    if code != ERROR_INSUFFICIENT_BUFFER.0 && code != 0 {
        return Err(denied());
    }
    for _ in 0..3 {
        if !(4..=4_194_304).contains(&size) {
            return Err(denied());
        }
        let mut data = vec![0u32; (size as usize).div_ceil(4)];
        let code = unsafe {
            GetExtendedTcpTable(
                Some(data.as_mut_ptr().cast()),
                &mut size,
                false,
                family,
                TCP_TABLE_OWNER_PID_ALL,
                0,
            )
        };
        if code == ERROR_INSUFFICIENT_BUFFER.0 {
            continue;
        }
        if code != 0 || size as usize > data.len() * 4 || size < 4 {
            return Err(denied());
        }
        data.truncate((size as usize).div_ceil(4));
        return Ok(data);
    }
    Err(denied())
}

fn rows<T: Copy>(data: &[u32], offset: usize) -> io::Result<Vec<T>> {
    let count = *data.first().ok_or_else(denied)? as usize;
    let end = count
        .checked_mul(size_of::<T>())
        .and_then(|size| size.checked_add(offset))
        .ok_or_else(denied)?;
    if end > std::mem::size_of_val(data) {
        return Err(denied());
    }
    // The native API initialized these complete rows. read_unaligned avoids
    // assuming more alignment than the u32 table allocation guarantees.
    Ok((0..count)
        .map(|i| unsafe {
            data.as_ptr()
                .cast::<u8>()
                .add(offset + i * size_of::<T>())
                .cast::<T>()
                .read_unaligned()
        })
        .collect())
}

fn port(value: u32) -> u16 {
    u16::from_be(value as u16)
}

fn owner_pid(connection: &Connection) -> io::Result<u32> {
    let mut matched = None;
    let mut accept =
        |local: SocketAddr, remote: SocketAddr, state: u32, pid: u32| -> io::Result<()> {
            if local == connection.peer
                && remote == connection.local
                && state == MIB_TCP_STATE_ESTAB.0 as u32
            {
                if pid == 0 || matched.replace(pid).is_some() {
                    return Err(denied());
                }
            }
            Ok(())
        };
    if connection.peer.is_ipv4() {
        for row in
            rows::<MIB_TCPROW_OWNER_PID>(&table(2)?, offset_of!(MIB_TCPTABLE_OWNER_PID, table))?
        {
            accept(
                SocketAddr::new(
                    Ipv4Addr::from(row.dwLocalAddr.to_ne_bytes()).into(),
                    port(row.dwLocalPort),
                ),
                SocketAddr::new(
                    Ipv4Addr::from(row.dwRemoteAddr.to_ne_bytes()).into(),
                    port(row.dwRemotePort),
                ),
                row.dwState,
                row.dwOwningPid,
            )?;
        }
    } else {
        for row in
            rows::<MIB_TCP6ROW_OWNER_PID>(&table(23)?, offset_of!(MIB_TCP6TABLE_OWNER_PID, table))?
        {
            let local = std::net::SocketAddrV6::new(
                Ipv6Addr::from(row.ucLocalAddr),
                port(row.dwLocalPort),
                0,
                row.dwLocalScopeId,
            );
            let remote = std::net::SocketAddrV6::new(
                Ipv6Addr::from(row.ucRemoteAddr),
                port(row.dwRemotePort),
                0,
                row.dwRemoteScopeId,
            );
            accept(local.into(), remote.into(), row.dwState, row.dwOwningPid)?;
        }
    }
    matched.ok_or_else(denied)
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{App, HttpResponse, HttpServer, test as actix_test, web};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn request_headers_and_claimed_peer_do_not_create_an_os_identity() {
        let request = actix_test::TestRequest::get()
            .peer_addr("127.0.0.1:12345".parse().unwrap())
            .insert_header(("Host", "localhost"))
            .insert_header(("Origin", "http://localhost"))
            .insert_header(("X-Forwarded-For", "127.0.0.1"))
            .to_http_request();
        assert!(authenticate(&request).is_err());
    }

    #[actix_web::test]
    async fn native_loopback_connections_identify_the_process_user_on_both_families() {
        let expected = crate::file_recovery_service::platform_user::current()
            .expect("native local-user test requires an interactive Windows user");
        for host in ["127.0.0.1", "::1"] {
            let server = HttpServer::new(|| {
                App::new().route(
                    "/",
                    web::get().to(|request: HttpRequest| async move {
                        match authenticate(&request) {
                            Ok(user) if user.is_alive() => HttpResponse::Ok().body(user.sid),
                            _ => HttpResponse::Forbidden().finish(),
                        }
                    }),
                )
            })
            .workers(1)
            .disable_signals()
            .on_connect(on_connect)
            .bind((host, 0))
            .unwrap();
            let address = server.addrs()[0];
            let server = server.run();
            let handle = server.handle();
            let task = actix_web::rt::spawn(server);
            let result = tokio::time::timeout(std::time::Duration::from_secs(10), async {
                let mut stream = TcpStream::connect(address).await?;
                stream
                    .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                    .await?;
                let mut response = Vec::new();
                stream.read_to_end(&mut response).await?;
                String::from_utf8(response).map_err(io::Error::other)
            })
            .await;
            // Stop only the ephemeral server created above, including on failure.
            handle.stop(false).await;
            task.await.unwrap().unwrap();
            let response = result
                .expect("loopback identity request timed out")
                .unwrap();
            assert!(
                response.starts_with("HTTP/1.1 200"),
                "identity lookup failed for {host}"
            );
            assert!(response.ends_with(&expected));
        }
    }
}
