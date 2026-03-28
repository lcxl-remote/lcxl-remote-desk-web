use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use bytes::Bytes;
use desk_utils::error::DeskErrorCode;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;
use webrtc::data_channel::RTCDataChannel;
use webrtc::data_channel::data_channel_message::DataChannelMessage;

use crate::error::DeskError;
use crate::model::file_transfer::*;
use crate::model::security_approval::{
    SecurityApprovalSender, SecurityPermissionType, check_security_permission,
};
use crate::model::settings::SharedSettings;

/// State for an active upload transfer
struct UploadState {
    file: tokio::fs::File,
    file_path: PathBuf,
    file_size: u64,
    total_chunks: u64,
    received_chunks: u64,
}

/// Handle file_transfer_event data channel
pub async fn handle_file_transfer_event(
    data_channel: Arc<RTCDataChannel>,
    settings: actix_web::web::Data<SharedSettings>,
    security_approval_sender: Option<SecurityApprovalSender>,
    connection_id: String,
) -> Result<(), DeskError> {
    let d_label = data_channel.label().to_owned();
    let d_id = data_channel.id();

    data_channel.on_close(Box::new(move || {
        log::info!("File transfer data channel closed");
        Box::pin(async {})
    }));

    // Shared state for active upload transfers
    let upload_states: Arc<Mutex<HashMap<String, UploadState>>> =
        Arc::new(Mutex::new(HashMap::new()));

    // Shared set of cancelled transfer IDs (for download cancellation)
    let cancelled_transfers: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));

    let dc_for_open = Arc::clone(&data_channel);
    data_channel.on_open(Box::new(move || {
        log::info!("File transfer data channel '{d_label}'-'{d_id}' open");
        let _dc = dc_for_open;
        Box::pin(async {})
    }));

    let dc_for_msg = Arc::clone(&data_channel);
    let upload_states_for_msg = upload_states.clone();
    let cancelled_for_msg = cancelled_transfers.clone();

    // Auth cache for DataChannel lifetime
    let permission_cache = Arc::new(tokio::sync::RwLock::new(None::<bool>));

    data_channel.on_message(Box::new(move |msg: DataChannelMessage| {
        let dc = dc_for_msg.clone();
        let upload_states = upload_states_for_msg.clone();
        let cancelled = cancelled_for_msg.clone();

        let settings = settings.clone();
        let sender = security_approval_sender.clone();
        let connection_id = connection_id.clone();
        let permission_cache = permission_cache.clone();

        Box::pin(async move {
            // Check permission first
            let mut allowed = false;
            let from_connection_id = connection_id.clone();
            {
                let cache = permission_cache.read().await;
                if let Some(res) = *cache {
                    allowed = res;
                } else {
                    drop(cache);
                    let mut cache_write = permission_cache.write().await;
                    if let Some(res) = *cache_write {
                        allowed = res;
                    } else {
                        let allow_transfer = { settings.read().await.security.allow_file_transfer };
                        let approved = check_security_permission(
                            &settings,
                            sender.as_ref(),
                            allow_transfer,
                            SecurityPermissionType::FileTransfer,
                            Some(connection_id),
                        )
                        .await;
                        *cache_write = Some(approved);
                        allowed = approved;
                    }
                }
            }
            if !allowed {
                log::warn!(
                    "File transfer message blocked by security settings or user for {}",
                    from_connection_id
                );
                return;
            }

            let result = if msg.is_string {
                // Text message: JSON control message
                handle_text_message(&dc, &msg.data, &upload_states, &cancelled).await
            } else {
                // Binary message: file chunk data
                handle_binary_message(&dc, &msg.data, &upload_states).await
            };

            if let Err(e) = result {
                log::error!("File transfer error: {}", e);
            }
        })
    }));

    Ok(())
}

/// Handle a JSON text control message
async fn handle_text_message(
    dc: &Arc<RTCDataChannel>,
    data: &Bytes,
    upload_states: &Arc<Mutex<HashMap<String, UploadState>>>,
    cancelled_transfers: &Arc<Mutex<HashSet<String>>>,
) -> Result<(), DeskError> {
    let msg_str = String::from_utf8(data.to_vec())?;

    let message: FileTransferMessage = serde_json::from_str(&msg_str)?;

    match message {
        FileTransferMessage::DownloadRequest(req) => {
            let dc = dc.clone();
            let cancelled = cancelled_transfers.clone();
            // Spawn the download task so it doesn't block message handling
            // (this allows cancel messages to be received while downloading)
            tokio::spawn(async move {
                if let Err(e) = handle_download_request(&dc, req, &cancelled).await {
                    log::error!("Download request error: {}", e);
                }
            });
        }
        FileTransferMessage::UploadRequest(req) => {
            handle_upload_request(dc, req, upload_states).await?;
        }
        FileTransferMessage::TransferComplete(complete) => {
            // Upload complete from browser side
            let mut states = upload_states.lock().await;
            if let Some(state) = states.remove(&complete.transfer_id) {
                log::info!(
                    "Upload transfer {} completed, received {} chunks",
                    complete.transfer_id,
                    state.received_chunks
                );
                drop(state);
            }
        }
        FileTransferMessage::TransferCancel(cancel) => {
            log::info!("Transfer cancel requested: {}", cancel.transfer_id);
            // Mark transfer as cancelled — the download loop will check this
            cancelled_transfers
                .lock()
                .await
                .insert(cancel.transfer_id.clone());
            // Remove upload state and delete the partially uploaded file
            if let Some(state) = upload_states.lock().await.remove(&cancel.transfer_id) {
                let file_path = state.file_path.clone();
                // Drop the file handle first so the OS releases the file
                drop(state);
                if let Err(e) = tokio::fs::remove_file(&file_path).await {
                    log::warn!(
                        "Failed to delete cancelled upload file {}: {}",
                        file_path.display(),
                        e
                    );
                } else {
                    log::info!("Deleted cancelled upload file: {}", file_path.display());
                }
            }
        }
        _ => {
            log::warn!(
                "Unexpected file transfer message type from browser: {:?}",
                message
            );
        }
    }

    Ok(())
}

/// Handle a binary data chunk message
async fn handle_binary_message(
    dc: &Arc<RTCDataChannel>,
    data: &Bytes,
    upload_states: &Arc<Mutex<HashMap<String, UploadState>>>,
) -> Result<(), DeskError> {
    let (transfer_id, chunk_index, chunk_data) = parse_binary_chunk(data).ok_or_else(|| {
        DeskError::new_custom_error(
            DeskErrorCode::SYSTEM_ERROR,
            "Invalid binary chunk: too short",
        )
    })?;

    let mut states = upload_states.lock().await;
    if let Some(state) = states.get_mut(transfer_id) {
        // Write chunk data to file
        state.file.write_all(chunk_data).await?;
        state.received_chunks += 1;

        log::debug!(
            "Upload transfer {}: received chunk {}/{}",
            transfer_id,
            chunk_index + 1,
            state.total_chunks
        );

        // Check if transfer is complete
        if state.received_chunks >= state.total_chunks {
            state.file.flush().await?;
            let transfer_id_owned = transfer_id.to_owned();
            states.remove(transfer_id);

            // Send transfer complete
            let complete_msg = FileTransferMessage::TransferComplete(TransferComplete {
                transfer_id: transfer_id_owned.clone(),
            });
            let json = serde_json::to_string(&complete_msg)?;
            dc.send_text(json).await?;
            log::info!(
                "Upload transfer {} completed successfully",
                transfer_id_owned
            );
        }
    } else {
        log::warn!(
            "Received binary chunk for unknown transfer: {}",
            transfer_id
        );
    }

    Ok(())
}

/// Handle a download request: read file and send chunks
async fn handle_download_request(
    dc: &Arc<RTCDataChannel>,
    req: DownloadRequest,
    cancelled_transfers: &Arc<Mutex<HashSet<String>>>,
) -> Result<(), DeskError> {
    let path = PathBuf::from(&req.file_path);

    // Validate file exists and is a file
    if !path.exists() || !path.is_file() {
        let error_msg = FileTransferMessage::TransferError(TransferError {
            transfer_id: req.transfer_id.clone(),
            message: format!("File not found: {}", req.file_path),
        });
        let json = serde_json::to_string(&error_msg)?;
        dc.send_text(json).await?;
        return Ok(());
    }

    let metadata = tokio::fs::metadata(&path).await?;
    let file_size = metadata.len();
    let file_name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    const FILE_TRANSFER_CHUNK_SIZE: usize = 60 * 1024; // 60KB for better SCTP throughput
    let chunk_size = FILE_TRANSFER_CHUNK_SIZE;
    let total_chunks = (file_size + chunk_size as u64 - 1) / chunk_size as u64;

    // Send download response with metadata
    let response = FileTransferMessage::DownloadResponse(DownloadResponse {
        transfer_id: req.transfer_id.clone(),
        file_name: file_name.clone(),
        file_size,
        chunk_size,
        total_chunks,
    });
    let json = serde_json::to_string(&response)?;
    dc.send_text(json).await?;

    // Read and send file chunks
    let mut file = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(e) => {
            let error_msg = FileTransferMessage::TransferError(TransferError {
                transfer_id: req.transfer_id.clone(),
                message: format!("Failed to open file: {}", e),
            });
            let json = serde_json::to_string(&error_msg)?;
            dc.send_text(json).await?;
            return Err(DeskError::from(e));
        }
    };
    let mut buf = vec![0u8; chunk_size];
    let mut chunk_index: u32 = 0;
    let mut last_log_time = std::time::Instant::now();

    log::info!(
        "Starting download for {}: {} chunks",
        req.transfer_id,
        total_chunks
    );

    loop {
        // Check if this transfer has been cancelled
        if cancelled_transfers.lock().await.remove(&req.transfer_id) {
            log::info!("Download transfer {} cancelled by client", req.transfer_id);
            return Ok(());
        }

        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }

        let chunk_bytes = build_binary_chunk(&req.transfer_id, chunk_index, &buf[..n]);

        // Backpressure & Debug Sampling: Check every 10 chunks
        if chunk_index % 10 == 0 {
            let current_buffered = dc.buffered_amount().await;

            // Periodic debug logging to see actual congestion
            if chunk_index % 100 == 0 {
                log::info!(
                    "Download transfer {}: current buffered_amount={} bytes",
                    req.transfer_id,
                    current_buffered
                );
            }

            if current_buffered > 2 * 1024 * 1024 {
                log::warn!(
                    "Download backpressure triggered for {}: buffered_amount={} bytes, pausing...",
                    req.transfer_id,
                    current_buffered
                );
                while dc.buffered_amount().await > 512 * 1024 {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                log::info!("Download backpressure released for {}", req.transfer_id);
            }
        }

        let send_start = std::time::Instant::now();
        dc.send(&Bytes::from(chunk_bytes)).await?;
        let send_duration = send_start.elapsed();

        // Performance monitoring & yield to allow SCTP stack to process ACKs
        if chunk_index % 100 == 0 {
            let current_buffered = dc.buffered_amount().await;
            log::info!(
                "Download performance sampling {}: send_took={:?}, buffered_amount={} bytes",
                req.transfer_id,
                send_duration,
                current_buffered
            );
            // Yield to avoid starving the SCTP internal loop
            tokio::task::yield_now().await;
        }

        if last_log_time.elapsed() >= std::time::Duration::from_secs(1) {
            log::debug!(
                "Download transfer {}: sent chunk {}/{}",
                req.transfer_id,
                chunk_index + 1,
                total_chunks
            );
            last_log_time = std::time::Instant::now();
        }

        chunk_index += 1;
    }

    // Send transfer complete
    let complete_msg = FileTransferMessage::TransferComplete(TransferComplete {
        transfer_id: req.transfer_id.clone(),
    });
    let json = serde_json::to_string(&complete_msg)?;
    dc.send_text(json).await?;

    log::info!(
        "Download transfer {} completed: {} ({} bytes, {} chunks)",
        req.transfer_id,
        file_name,
        file_size,
        chunk_index
    );

    Ok(())
}

/// Handle an upload request: prepare to receive file chunks
async fn handle_upload_request(
    dc: &Arc<RTCDataChannel>,
    req: UploadRequest,
    upload_states: &Arc<Mutex<HashMap<String, UploadState>>>,
) -> Result<(), DeskError> {
    let target_dir = PathBuf::from(&req.target_dir);

    // Validate target directory
    if !target_dir.exists() || !target_dir.is_dir() {
        let error_msg = FileTransferMessage::TransferError(TransferError {
            transfer_id: req.transfer_id.clone(),
            message: format!("Target directory not found: {}", req.target_dir),
        });
        let json = serde_json::to_string(&error_msg)?;
        dc.send_text(json).await?;
        return Ok(());
    }

    let file_path = target_dir.join(&req.file_name);
    let file = tokio::fs::File::create(&file_path).await?;

    log::info!(
        "Upload transfer {} started: {} -> {} ({} bytes, {} chunks)",
        req.transfer_id,
        req.file_name,
        file_path.display(),
        req.file_size,
        req.total_chunks
    );

    // Store upload state
    let state = UploadState {
        file,
        file_path: file_path.clone(),
        file_size: req.file_size,
        total_chunks: req.total_chunks,
        received_chunks: 0,
    };
    upload_states
        .lock()
        .await
        .insert(req.transfer_id.clone(), state);

    // Send upload response
    let response = FileTransferMessage::UploadResponse(UploadResponse {
        transfer_id: req.transfer_id.clone(),
        accepted: true,
        message: None,
    });
    let json = serde_json::to_string(&response)?;
    dc.send_text(json).await?;

    Ok(())
}
