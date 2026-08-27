use std::io;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use wincode::config::{Configuration, PREALLOCATION_SIZE_LIMIT_DISABLED};

/// Maximum message size: 16 MB
const MAX_MESSAGE_SIZE: u32 = 16 * 1024 * 1024;

/// Wincode `Configuration` used for every IPC frame on the daemon ↔ worker
/// transport.
///
/// `PREALLOCATION_SIZE_LIMIT_DISABLED` is required: wincode's default 4 MiB
/// preallocation guard fires on both encode and decode, so a 4K IDR frame
/// (~2 MB) plus a multi-MB whiteboard / file blob (up to the
/// transport-layer 16 MB ceiling) would otherwise be rejected by the
/// serializer before `MAX_MESSAGE_SIZE` ever sees it. We rely on the
/// transport-layer `MAX_MESSAGE_SIZE` check below for the upper bound and
/// disable the wincode-internal guard.
pub type IpcConfigType = Configuration<true, PREALLOCATION_SIZE_LIMIT_DISABLED>;
pub const IPC_CONFIG: IpcConfigType = Configuration::new();

/// Write a length-prefixed wincode message to an async writer.
///
/// Wire format: little-endian `u32` length, followed by `length` bytes of
/// wincode-encoded payload (FixInt + little-endian, preallocation limit
/// disabled — see [`IPC_CONFIG`]).
///
/// Frame size cap: `MAX_MESSAGE_SIZE` (16 MB).
pub async fn write_message<W, M>(writer: &mut W, message: &M) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
    M: wincode::SchemaWrite<IpcConfigType, Src = M>,
{
    let bytes = wincode::config::serialize(message, IPC_CONFIG).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Failed to encode message (wincode): {e}"),
        )
    })?;

    if bytes.len() > MAX_MESSAGE_SIZE as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "Message too large: {} bytes (max {})",
                bytes.len(),
                MAX_MESSAGE_SIZE
            ),
        ));
    }
    let len = bytes.len() as u32;

    writer.write_all(&len.to_le_bytes()).await?;
    writer.write_all(&bytes).await?;
    writer.flush().await?;

    Ok(())
}

/// Read a length-prefixed wincode message from an async reader.
///
/// See [`write_message`] for the wire format.
pub async fn read_message<R, M>(reader: &mut R) -> io::Result<M>
where
    R: AsyncRead + Unpin,
    M: for<'de> wincode::SchemaRead<'de, IpcConfigType, Dst = M>,
{
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).await?;
    let len = u32::from_le_bytes(len_buf);

    if len > MAX_MESSAGE_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "Message too large: {} bytes (max {})",
                len, MAX_MESSAGE_SIZE
            ),
        ));
    }

    let mut buf = vec![0u8; len as usize];
    reader.read_exact(&mut buf).await?;

    let msg = wincode::config::deserialize(&buf, IPC_CONFIG).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Failed to decode message (wincode): {e}"),
        )
    })?;
    Ok(msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{
        OpaqueConnectionPayload, ServiceToWorker, WorkerInitPayload, WorkerToService,
    };

    #[tokio::test]
    async fn roundtrip_init() {
        let msg = ServiceToWorker::Init(WorkerInitPayload {
            session_id: "test-session".to_string(),
            os_session_id: 1,
            desktop_name: Some("Default".to_string()),
            config_json: "{}".to_string(),
            log_dir: None,
            signaling_url: None,
            auth_token: None,
            host_upstream_url: None,
            media_pipe_name: None,
            file_pipe_name: None,
            remote_access_locked: false,
            remote_access_state_version: 1,
        });

        let mut buf = Vec::new();
        write_message(&mut buf, &msg).await.unwrap();

        let mut cursor = std::io::Cursor::new(buf);
        let decoded: ServiceToWorker = read_message(&mut cursor).await.unwrap();

        match decoded {
            ServiceToWorker::Init(payload) => {
                assert_eq!(payload.session_id, "test-session");
                assert_eq!(payload.os_session_id, 1);
                assert_eq!(payload.desktop_name, Some("Default".to_string()));
            }
            _ => panic!("Expected Init message"),
        }
    }

    /// Worker-to-service path round-trips correctly.
    #[tokio::test]
    async fn roundtrip_ready() {
        let msg = WorkerToService::Ready;
        let mut buf = Vec::new();
        write_message(&mut buf, &msg).await.unwrap();

        let mut cursor = std::io::Cursor::new(buf);
        let decoded: WorkerToService = read_message(&mut cursor).await.unwrap();
        assert!(matches!(decoded, WorkerToService::Ready));
    }

    /// Length prefix is little-endian and reflects the encoded payload size,
    /// not the raw struct size.
    #[tokio::test]
    async fn length_prefix_is_le_u32() {
        let msg = WorkerToService::Ready;
        let mut buf = Vec::new();
        write_message(&mut buf, &msg).await.unwrap();
        assert!(buf.len() >= 4);
        let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
        assert_eq!(len, buf.len() - 4);
    }

    /// Frames larger than `MAX_MESSAGE_SIZE` (16 MB) are rejected at write
    /// time, before any bytes go on the wire.
    #[tokio::test]
    async fn write_rejects_oversized_frame() {
        // Construct a payload that, after wincode encoding, exceeds 16 MB.
        let huge_blob = "x".repeat(20 * 1024 * 1024);
        let msg = ServiceToWorker::Init(WorkerInitPayload {
            session_id: "s".to_string(),
            os_session_id: 1,
            desktop_name: None,
            config_json: huge_blob,
            log_dir: None,
            signaling_url: None,
            auth_token: None,
            host_upstream_url: None,
            media_pipe_name: None,
            file_pipe_name: None,
            remote_access_locked: false,
            remote_access_state_version: 1,
        });
        let mut buf = Vec::new();
        let err = write_message(&mut buf, &msg).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(
            buf.is_empty(),
            "no bytes should be written on oversized frame"
        );
    }

    /// A length prefix that exceeds `MAX_MESSAGE_SIZE` is rejected at read
    /// time, before the body is read into memory.
    #[tokio::test]
    async fn read_rejects_oversized_length_prefix() {
        let bad_len: u32 = MAX_MESSAGE_SIZE + 1;
        let mut wire = bad_len.to_le_bytes().to_vec();
        // Body intentionally absent — read should fail on length check, not EOF.
        let mut cursor = std::io::Cursor::new(wire.clone());
        let err = read_message::<_, WorkerToService>(&mut cursor)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        // No bytes beyond the prefix should have been consumed.
        wire.clear();
    }

    /// A payload comfortably above wincode's 4 MiB default
    /// preallocation guard but below the 16 MB transport ceiling must
    /// round-trip via `write_message` + `read_message`. If anything in
    /// the IPC path falls back to a default `wincode::serialize` /
    /// `wincode::deserialize` (instead of `wincode::config::serialize`
    /// + [`IPC_CONFIG`]), the preallocation guard fires on encode and
    /// this test fails — that is the "did we wire `IPC_CONFIG` in
    /// everywhere" gold-standard assertion.
    #[tokio::test]
    async fn frame_between_4mib_and_16mb_round_trips() {
        let payload = vec![0xABu8; 8 * 1024 * 1024]; // 8 MiB
        let original = ServiceToWorker::WhiteboardCommand(OpaqueConnectionPayload {
            connection_id: "conn-large".to_string(),
            data: payload.clone(),
        });

        let mut buf = Vec::new();
        write_message(&mut buf, &original).await.expect("write");
        assert!(
            buf.len() > 4 * 1024 * 1024,
            "wire frame should exceed 4 MiB to exercise the preallocation guard"
        );
        assert!(
            buf.len() < MAX_MESSAGE_SIZE as usize,
            "wire frame should stay below the 16 MB transport ceiling"
        );

        let mut cursor = std::io::Cursor::new(buf);
        let decoded: ServiceToWorker = read_message(&mut cursor).await.expect("read");
        match decoded {
            ServiceToWorker::WhiteboardCommand(p) => {
                assert_eq!(p.connection_id, "conn-large");
                assert_eq!(p.data.len(), payload.len());
                assert_eq!(&p.data[..16], &payload[..16]);
                assert_eq!(&p.data[p.data.len() - 16..], &payload[payload.len() - 16..]);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }
}
