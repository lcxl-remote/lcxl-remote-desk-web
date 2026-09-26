//! Persist addressing separately from authentication and connection incarnation.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io,
    net::TcpListener,
    path::{Path, PathBuf},
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Endpoint {
    version: u16,
    port: u16,
}

fn path(root: &Path, device: &str, suffix: &str) -> PathBuf {
    root.join(format!(
        "browser-bridge-{:x}.{suffix}",
        Sha256::digest(device.as_bytes())
    ))
}

pub(super) struct BoundEndpoint {
    pub listener: TcpListener,
    pub lock: File,
}

pub(super) fn bind(root: &Path, device: &str) -> io::Result<BoundEndpoint> {
    let lock = super::private_file::open(&path(root, device, "lock"), true, 128)?;
    lock.try_lock().map_err(|error| {
        io::Error::other(format!(
            "browser bridge already owned or lock unavailable: {error}"
        ))
    })?;
    let record_path = path(root, device, "json");
    let saved = read_record(&record_path)?;
    // Port conflicts disable this bridge. Never trust or connect to the occupant.
    let listener = TcpListener::bind((
        std::net::Ipv4Addr::LOCALHOST,
        saved.map_or(0, |entry| entry.port),
    ))?;
    let port = listener.local_addr()?.port();
    super::private_file::write(
        &record_path,
        &serde_json::to_vec(&Endpoint { version: 2, port })?,
    )?;
    Ok(BoundEndpoint { listener, lock })
}

fn read_record(path: &Path) -> io::Result<Option<Endpoint>> {
    let bytes = match super::private_file::read(path, 128) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let record: Endpoint = serde_json::from_slice(&bytes)?;
    if record.version != 2 || record.port == 0 {
        return Err(io::Error::other("invalid browser endpoint version or port"));
    }
    Ok(Some(record))
}

pub(super) fn url(root: &Path, device: &str) -> io::Result<String> {
    let record = read_record(&path(root, device, "json"))?
        .ok_or_else(|| io::Error::other("browser endpoint is not initialized"))?;
    Ok(format!(
        "ws://127.0.0.1:{}/browser-extension/v2",
        record.port
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn restart_reuses_endpoint_and_duplicate_owner_is_rejected() {
        let root = crate::worker::agent::browser_extension_bridge::private_test_directory();
        let first = bind(root.path(), "device").unwrap();
        let address = first.listener.local_addr().unwrap();
        assert!(bind(root.path(), "device").is_err());
        drop(first);
        let second = bind(root.path(), "device").unwrap();
        assert_eq!(second.listener.local_addr().unwrap(), address);
    }
    #[test]
    fn occupied_saved_port_fails_without_changing_pairing_address() {
        let root = crate::worker::agent::browser_extension_bridge::private_test_directory();
        let first = bind(root.path(), "device").unwrap();
        let address = first.listener.local_addr().unwrap();
        drop(first);
        let _occupant = TcpListener::bind(address).unwrap();
        assert_eq!(
            bind(root.path(), "device").err().unwrap().kind(),
            io::ErrorKind::AddrInUse
        );
        assert!(
            url(root.path(), "device")
                .unwrap()
                .contains(&address.port().to_string())
        );
    }
}
