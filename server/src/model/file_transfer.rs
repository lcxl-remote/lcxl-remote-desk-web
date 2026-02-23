use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

pub const FILE_TRANSFER_CHUNK_SIZE: usize = 32 * 1024;

/// File transfer message types (JSON text messages over DataChannel)
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FileTransferMessage {
    /// Browser → Server: Request to download a file
    DownloadRequest(DownloadRequest),
    /// Server → Browser: Response with file metadata
    DownloadResponse(DownloadResponse),
    /// Browser → Server: Request to upload a file
    UploadRequest(UploadRequest),
    /// Server → Browser: Response confirming upload
    UploadResponse(UploadResponse),
    /// Either direction: Transfer completed
    TransferComplete(TransferComplete),
    /// Either direction: Transfer error
    TransferError(TransferError),
    /// Either direction: Cancel an active transfer
    TransferCancel(TransferCancel),
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct DownloadRequest {
    pub transfer_id: String,
    pub file_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct DownloadResponse {
    pub transfer_id: String,
    pub file_name: String,
    pub file_size: u64,
    pub chunk_size: usize,
    pub total_chunks: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UploadRequest {
    pub transfer_id: String,
    pub target_dir: String,
    pub file_name: String,
    pub file_size: u64,
    pub chunk_size: usize,
    pub total_chunks: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UploadResponse {
    pub transfer_id: String,
    pub accepted: bool,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct TransferComplete {
    pub transfer_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct TransferError {
    pub transfer_id: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct TransferCancel {
    pub transfer_id: String,
}

/// Binary data chunk header:
/// - transfer_id: 36 bytes (UUID string as UTF-8)
/// - chunk_index: 4 bytes (u32 big-endian)
/// - remaining: file data
pub const BINARY_HEADER_SIZE: usize = 36 + 4;

/// Parse a binary chunk header from raw bytes.
/// Returns (transfer_id, chunk_index, data_slice).
pub fn parse_binary_chunk(data: &[u8]) -> Option<(&str, u32, &[u8])> {
    if data.len() < BINARY_HEADER_SIZE {
        return None;
    }
    let transfer_id = std::str::from_utf8(&data[..36]).ok()?;
    let chunk_index = u32::from_be_bytes([data[36], data[37], data[38], data[39]]);
    Some((transfer_id, chunk_index, &data[BINARY_HEADER_SIZE..]))
}

/// Build a binary chunk with header prepended.
pub fn build_binary_chunk(transfer_id: &str, chunk_index: u32, data: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(BINARY_HEADER_SIZE + data.len());
    // transfer_id must be exactly 36 bytes (UUID format)
    let id_bytes = transfer_id.as_bytes();
    debug_assert_eq!(id_bytes.len(), 36);
    buf.extend_from_slice(id_bytes);
    buf.extend_from_slice(&chunk_index.to_be_bytes());
    buf.extend_from_slice(data);
    buf
}
