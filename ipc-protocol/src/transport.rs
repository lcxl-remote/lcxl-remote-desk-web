use std::io;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Maximum message size: 16 MB
const MAX_MESSAGE_SIZE: u32 = 16 * 1024 * 1024;

/// Write a length-prefixed JSON message to an async writer.
pub async fn write_message<W, M>(writer: &mut W, message: &M) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
    M: Serialize,
{
    let json = serde_json::to_vec(message).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Failed to serialize message: {}", e),
        )
    })?;

    let len = json.len() as u32;
    if len > MAX_MESSAGE_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "Message too large: {} bytes (max {})",
                len, MAX_MESSAGE_SIZE
            ),
        ));
    }

    writer.write_all(&len.to_le_bytes()).await?;
    writer.write_all(&json).await?;
    writer.flush().await?;

    Ok(())
}

/// Read a length-prefixed JSON message from an async reader.
pub async fn read_message<R, M>(reader: &mut R) -> io::Result<M>
where
    R: AsyncRead + Unpin,
    M: for<'de> Deserialize<'de>,
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

    serde_json::from_slice(&buf).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Failed to deserialize message: {}", e),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{ServiceToWorker, WorkerInitPayload};

    #[tokio::test]
    async fn test_roundtrip() {
        let msg = ServiceToWorker::Init(WorkerInitPayload {
            session_id: "test-session".to_string(),
            os_session_id: 1,
            desktop_name: Some("Default".to_string()),
            config_json: "{}".to_string(),
            signaling_url: None,
            auth_token: None,
            host_upstream_url: None,
            preapproved_connections: Vec::new(),
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
}
