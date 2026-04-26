use std::{
    hash::{DefaultHasher, Hash, Hasher},
    sync::Arc,
    time::Duration,
};

use base64::Engine;
use base64::prelude::*;
use desk_signal_facade::model::signal::SignalingState;
use tokio::sync::{Mutex, RwLock};
use webrtc::data_channel::RTCDataChannel;

use crate::{
    error::DeskError,
    model::data_channel::ClipboardEventData,
};
use desk_input_injection::model::host_control::{ClipboardImage, HostControlHelper};
use desk_input_injection::host_control::host_control_factory::create_host_control_helper;

// 1MB max for text
const MAX_TEXT_SIZE: usize = 1024 * 1024;
// 25MB max for image
const MAX_IMAGE_SIZE: usize = 25 * 1024 * 1024;

#[derive(Default)]
struct ImageTransferState {
    pub total_bytes: u64,
    pub chunks_received: u32,
    pub total_chunks: u32,
    pub data: Vec<u8>,
}

pub async fn handle_clipboard_event(
    signaling_state: Arc<RwLock<SignalingState>>,
    data_channel: Arc<RTCDataChannel>,
) -> Result<(), DeskError> {
    let _d_label = data_channel.label().to_owned();
    let _d_id = data_channel.id();

    let desk_settings = {
        let state = signaling_state.read().await;
        // The display_info is available, we assume DeskSettings can be reconstructed or we pass a default one.
        // Or we use create_host_control_helper with default or existing settings.
        desk_signal_facade::model::desk_settings::DeskSettings::default()
    };

    let setting_helper: Arc<Mutex<Box<dyn HostControlHelper + Send + Sync>>> = Arc::new(
        Mutex::new(create_host_control_helper(&desk_settings, None)?),
    );

    // Last hash we pushed to local system clipboard, to prevent echo loop
    let last_written_hash: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));

    // Chunk assembler state
    let image_state: Arc<Mutex<Option<ImageTransferState>>> = Arc::new(Mutex::new(None));

    // Wait until DC is open
    let dc_sender = Arc::clone(&data_channel);
    let state_checker = Arc::clone(&signaling_state);
    let helper_checker = Arc::clone(&setting_helper);
    let hash_checker = Arc::clone(&last_written_hash);

    data_channel.on_open(Box::new(move || {
        log::info!("Clipboard data channel open. Starting polling task.");
        let dc = Arc::clone(&dc_sender);
        let s_state = Arc::clone(&state_checker);
        let settings_h = Arc::clone(&helper_checker);
        let w_hash = Arc::clone(&hash_checker);

        Box::pin(async move {
            let mut last_pushed_hash: Option<u64> = None;
            loop {
                tokio::time::sleep(Duration::from_millis(500)).await;

                // Stop if DC is closed
                if dc.ready_state()
                    != webrtc::data_channel::data_channel_state::RTCDataChannelState::Open
                {
                    break;
                }

                // Stop if control or clipboard sync not accepted
                let accepted = {
                    let st = s_state.read().await;
                    st.accept_control && st.accept_clipboard_sync
                };
                if !accepted {
                    continue;
                }

                let mut helper = settings_h.lock().await;

                // 1. Check text first
                if let Ok(Some(text)) = helper.get_text_from_clipboard() {
                    let mut hasher = DefaultHasher::new();
                    text.hash(&mut hasher);
                    let current_hash = hasher.finish();

                    let w_hash_val = *w_hash.lock().await;
                    if Some(current_hash) == w_hash_val {
                        // Echo loop prevention, skip
                        continue;
                    }
                    if Some(current_hash) == last_pushed_hash {
                        // Already pushed, skip
                        continue;
                    }

                    if text.len() > MAX_TEXT_SIZE {
                        log::warn!("Local clipboard text too large to send (>1MB)");
                        let msg = ClipboardEventData {
                            r#type: "error".to_string(),
                            content: Some("Text too large to sync (>1MB)".to_string()),
                            width: None,
                            height: None,
                            total_bytes: None,
                            chunk_count: None,
                            index: None,
                        };
                        let _ = dc.send_text(serde_json::to_string(&msg).unwrap()).await;
                        last_pushed_hash = Some(current_hash);
                        continue;
                    }

                    // Send text
                    let msg = ClipboardEventData {
                        r#type: "text".to_string(),
                        content: Some(text),
                        width: None,
                        height: None,
                        total_bytes: None,
                        chunk_count: None,
                        index: None,
                    };
                    if let Ok(json_str) = serde_json::to_string(&msg) {
                        let _ = dc.send_text(json_str).await;
                        last_pushed_hash = Some(current_hash);
                    }
                    continue; // Skip image check if text is present and updated
                }

                // 2. Check Image if text is None
                if let Ok(Some(image)) = helper.get_image_from_clipboard() {
                    let mut hasher = DefaultHasher::new();
                    image.bytes.hash(&mut hasher);
                    hasher.write_usize(image.width);
                    hasher.write_usize(image.height);
                    let current_hash = hasher.finish();

                    let w_hash_val = *w_hash.lock().await;
                    if Some(current_hash) == w_hash_val {
                        continue;
                    }
                    if Some(current_hash) == last_pushed_hash {
                        continue;
                    }

                    // We need to encode it to PNG
                    let mut png_data = Vec::new();
                    {
                        let mut encoder = png::Encoder::new(
                            &mut png_data,
                            image.width as u32,
                            image.height as u32,
                        );
                        encoder.set_color(png::ColorType::Rgba);
                        encoder.set_depth(png::BitDepth::Eight);
                        if let Ok(mut writer) = encoder.write_header() {
                            let _ = writer.write_image_data(&image.bytes);
                        }
                    }

                    if png_data.len() > MAX_IMAGE_SIZE {
                        log::warn!("Local clipboard image too large to send (>25MB)");
                        let msg = ClipboardEventData {
                            r#type: "error".to_string(),
                            content: Some("Image too large to sync (>25MB)".to_string()),
                            width: None,
                            height: None,
                            total_bytes: None,
                            chunk_count: None,
                            index: None,
                        };
                        let _ = dc.send_text(serde_json::to_string(&msg).unwrap()).await;
                        last_pushed_hash = Some(current_hash);
                        continue;
                    }

                    let base64_str = BASE64_STANDARD.encode(&png_data);
                    let chunk_size = 32 * 1024; // 32KB per chunk for safe DataChannel transmit
                    let mut chunks = Vec::new();
                    let mut offset = 0;
                    while offset < base64_str.len() {
                        let end = std::cmp::min(offset + chunk_size, base64_str.len());
                        chunks.push(base64_str[offset..end].to_string());
                        offset = end;
                    }

                    let chunk_count = chunks.len() as u32;

                    // Send start
                    let start_msg = ClipboardEventData {
                        r#type: "image_start".to_string(),
                        content: None,
                        width: Some(image.width as u32),
                        height: Some(image.height as u32),
                        total_bytes: Some(png_data.len() as u64),
                        chunk_count: Some(chunk_count),
                        index: None,
                    };
                    let _ = dc
                        .send_text(serde_json::to_string(&start_msg).unwrap())
                        .await;

                    // Send chunks
                    for (i, chunk) in chunks.into_iter().enumerate() {
                        let chunk_msg = ClipboardEventData {
                            r#type: "image_chunk".to_string(),
                            content: Some(chunk),
                            width: None,
                            height: None,
                            total_bytes: None,
                            chunk_count: None,
                            index: Some(i as u32),
                        };
                        let _ = dc
                            .send_text(serde_json::to_string(&chunk_msg).unwrap())
                            .await;
                    }

                    // Send end
                    let end_msg = ClipboardEventData {
                        r#type: "image_end".to_string(),
                        content: None,
                        width: None,
                        height: None,
                        total_bytes: None,
                        chunk_count: None,
                        index: None,
                    };
                    let _ = dc.send_text(serde_json::to_string(&end_msg).unwrap()).await;

                    last_pushed_hash = Some(current_hash);
                }
            }
        })
    }));

    let signaling_state2 = signaling_state.clone();
    let dc_sender2 = Arc::clone(&data_channel);

    data_channel.on_message(Box::new(move |msg| {
        let dc = dc_sender2.clone();
        let s_state = signaling_state2.clone();
        let settings = setting_helper.clone();
        let w_hash = last_written_hash.clone();
        let img_state_arc = image_state.clone();

        let msg_str = String::from_utf8(msg.data.to_vec()).unwrap();
        Box::pin(async move {
            let accepted = {
                let st = s_state.read().await;
                st.accept_control && st.accept_clipboard_sync
            };
            if !accepted {
                return;
            }

            if let Ok(event) = serde_json::from_str::<ClipboardEventData>(&msg_str) {
                let mut helper = settings.lock().await;
                match event.r#type.as_str() {
                    "text" => {
                        if let Some(text) = event.content {
                            if text.len() > MAX_TEXT_SIZE {
                                log::warn!("Received text exceeds 1MB limit.");
                                return;
                            }
                            if let Err(e) = helper.set_text_to_clipboard(&text) {
                                log::error!("Failed to set text to clipboard: {}", e);
                            } else {
                                let mut hasher = DefaultHasher::new();
                                text.hash(&mut hasher);
                                *w_hash.lock().await = Some(hasher.finish());
                            }
                        }
                    }
                    "image_start" => {
                        let total = event.total_bytes.unwrap_or(0);
                        if total > MAX_IMAGE_SIZE as u64 {
                            log::warn!(
                                "Incoming image too large (>{}), rejecting.",
                                MAX_IMAGE_SIZE
                            );
                            *img_state_arc.lock().await = None;
                            return;
                        }
                        *img_state_arc.lock().await = Some(ImageTransferState {
                            total_bytes: total,
                            chunks_received: 0,
                            total_chunks: event.chunk_count.unwrap_or(0),
                            data: Vec::new(),
                        });
                    }
                    "image_chunk" => {
                        if let Some(mut state) = img_state_arc.lock().await.take() {
                            if let Some(ref chunk) = event.content {
                                state.data.extend_from_slice(chunk.as_bytes());
                                state.chunks_received += 1;
                            }
                            *img_state_arc.lock().await = Some(state);
                        }
                    }
                    "image_end" => {
                        if let Some(state) = img_state_arc.lock().await.take() {
                            if state.chunks_received == state.total_chunks {
                                // Decode base64
                                if let Ok(png_bytes) = BASE64_STANDARD.decode(&state.data) {
                                    // Parse PNG to get raw RGBA
                                    let cursor = std::io::Cursor::new(png_bytes);
                                    let decoder = png::Decoder::new(cursor);
                                    if let Ok(mut reader) = decoder.read_info() {
                                        let mut buf = vec![0; reader.output_buffer_size()];
                                        let info = reader.next_frame(&mut buf).unwrap();
                                        let img0 = ClipboardImage {
                                            width: info.width as usize,
                                            height: info.height as usize,
                                            bytes: std::borrow::Cow::Owned(
                                                buf[..info.buffer_size()].to_vec(),
                                            ),
                                        };
                                        if let Err(e) = helper.set_image_to_clipboard(&img0) {
                                            log::error!(
                                                "Failed to set image to local clipboard: {}",
                                                e
                                            );
                                        } else {
                                            let mut hasher = DefaultHasher::new();
                                            img0.bytes.hash(&mut hasher);
                                            hasher.write_usize(img0.width);
                                            hasher.write_usize(img0.height);
                                            *w_hash.lock().await = Some(hasher.finish());
                                        }
                                    } else {
                                        log::error!(
                                            "Failed to read PNG info from incoming chunks."
                                        );
                                    }
                                } else {
                                    log::error!("Failed to decode base64 clipboard image.");
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        })
    }));

    Ok(())
}
