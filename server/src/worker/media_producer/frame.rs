use super::*;

/// Diagnostic helper: emit one INFO line describing what the encoder
/// produced on the first emit pass after a fresh build (initial start,
/// settings_changed rebuild, or keyframe_requested rebuild). Helps
/// confirm whether `next_pass_is_idr=true` actually translated into an
/// IDR / SPS / PPS NAL on the wire vs. a non-IDR slice mis-labelled
/// VideoI. Decodes H.264 NAL unit type (`byte & 0x1F` after the
/// startcode); for other codecs only the first 8 payload bytes are
/// dumped — operators reading the log can correlate against codec
/// specs as needed.
pub(super) fn log_post_rebuild_emit(
    connection_id: &str,
    path: &str,
    codec: MediaCodec,
    kind: MediaFrameKind,
    next_pass_is_idr: bool,
    nal_bytes: &[u8],
) {
    let head_hex: String = nal_bytes
        .iter()
        .take(16)
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ");
    let codec_specific = match codec {
        MediaCodec::H264 => {
            // Walk all NAL units in the bytestream and list type +
            // length of each. The "screen turns green" investigation
            // hinges on whether a rebuild-IDR frame is `SPS + PPS +
            // real IDR slice` (~tens of KB for 1280x800) or `SPS +
            // PPS + empty / dummy slice` (only a few KB), so the
            // first-NAL-only summary isn't enough — we need every
            // NAL's identity to tell those apart.
            let nals = h264_walk_nals(nal_bytes);
            if nals.is_empty() {
                ", h264_nals=<no startcode>".to_string()
            } else {
                let parts: Vec<String> = nals
                    .iter()
                    .map(|(byte, len)| {
                        let unit_type = byte & 0x1F;
                        let label = match unit_type {
                            1 => "non-IDR",
                            5 => "IDR",
                            6 => "SEI",
                            7 => "SPS",
                            8 => "PPS",
                            9 => "AUD",
                            _ => "?",
                        };
                        format!("{unit_type}({label}):{len}")
                    })
                    .collect();
                format!(", h264_nals=[{}]", parts.join(", "))
            }
        }
        MediaCodec::Vp8 | MediaCodec::Vp9 => {
            let kf_bit = nal_bytes.first().map(|b| b & 0x01);
            match kf_bit {
                Some(0) => ", vpx_frame_type=key".to_string(),
                Some(_) => ", vpx_frame_type=inter".to_string(),
                None => ", vpx_frame_type=<empty>".to_string(),
            }
        }
        _ => String::new(),
    };
    info!(
        "[MediaProducer:{connection_id}] post-rebuild first emit (path={path}, kind={kind:?}, \
         next_pass_is_idr={next_pass_is_idr}, codec={codec:?}, payload_len={}, head={head_hex}{codec_specific})",
        nal_bytes.len()
    );
}

/// Walk an Annex-B H.264 bytestream and return `(header_byte, payload_len)`
/// for each NAL unit found. `payload_len` is the size of the NAL itself
/// (excluding the leading startcode), measured up to the next startcode
/// or end-of-buffer. Used purely for diagnostic logging.
pub(super) fn h264_walk_nals(nal_bytes: &[u8]) -> Vec<(u8, usize)> {
    let mut nals: Vec<(u8, usize)> = Vec::new();
    // Locate every Annex-B startcode (`00 00 00 01` or `00 00 01`) and
    // record its position + the size of the startcode prefix so we can
    // measure each NAL's payload length as the distance to the next
    // startcode (or end of buffer).
    let mut starts: Vec<(usize, usize)> = Vec::new(); // (offset_after_startcode, prefix_len)
    let mut i = 0;
    while i + 2 < nal_bytes.len() {
        if nal_bytes[i] == 0 && nal_bytes[i + 1] == 0 {
            if i + 3 < nal_bytes.len() && nal_bytes[i + 2] == 0 && nal_bytes[i + 3] == 1 {
                starts.push((i + 4, 4));
                i += 4;
                continue;
            }
            if nal_bytes[i + 2] == 1 {
                starts.push((i + 3, 3));
                i += 3;
                continue;
            }
        }
        i += 1;
    }
    for (idx, (off, _)) in starts.iter().enumerate() {
        if *off >= nal_bytes.len() {
            continue;
        }
        let header_byte = nal_bytes[*off];
        let next_start = starts
            .get(idx + 1)
            .map(|(o, p)| o.saturating_sub(*p))
            .unwrap_or(nal_bytes.len());
        let payload_len = next_start.saturating_sub(*off);
        nals.push((header_byte, payload_len));
    }
    nals
}

pub(super) fn build_media_frame(
    connection_id: &str,
    seq: u64,
    duration_ns: u64,
    kind: MediaFrameKind,
    codec: MediaCodec,
    payload: Vec<u8>,
) -> MediaFrame {
    let ts_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(u64::MAX as u128) as u64;
    MediaFrame {
        connection_id: connection_id.to_string(),
        seq,
        ts_ns,
        duration_ns,
        kind,
        codec,
        payload,
    }
}

/// Push a frame onto the media transport. Returns `false` when the
/// loop should exit (transport closed). I-frame send timeout surfaces
/// as a `WorkerToService::Error { MediaTransportStuck }` to the daemon
/// — the producer does not self-decide to abort, the daemon issues
/// `StopMedia`+`StartMedia` instead.
pub(super) async fn send_frame(
    media_sender: &Arc<dyn MediaSender>,
    error_tx: &mpsc::UnboundedSender<WorkerToService>,
    connection_id: &str,
    frame: MediaFrame,
) -> bool {
    let kind = frame.kind;
    match media_sender.send_frame(frame).await {
        Ok(()) => true,
        Err(TransportError::Closed) => {
            warn!(
                "[MediaProducer:{connection_id}] media transport closed; pipeline thread exiting"
            );
            false
        }
        Err(TransportError::Backpressured) => {
            // P-frame drop — request a fresh keyframe on the next
            // encode pass so the stream resyncs.
            debug!("[MediaProducer:{connection_id}] media transport backpressured; dropping frame");
            true
        }
        Err(TransportError::IFrameTimeout) => {
            error!(
                "[MediaProducer:{connection_id}] I-frame send timed out; surfacing \
                 MediaTransportStuck to daemon for reset"
            );
            // Carry `connection_id` on the payload so the daemon can issue
            // StopMedia + StartMedia for exactly this PC instead of having
            // to parse the human-readable `message` field.
            let _ = error_tx.send(WorkerToService::Error(ErrorPayload {
                code: ERROR_CODE_MEDIA_TRANSPORT_STUCK,
                message: format!(
                    "I-frame send timed out for connection {connection_id} (kind={kind:?}); \
                     daemon should issue StopMedia+StartMedia"
                ),
                recoverable: true,
                connection_id: Some(connection_id.to_string()),
            }));
            true
        }
        Err(other) => {
            warn!("[MediaProducer:{connection_id}] media transport send error: {other}");
            true
        }
    }
}

/// Send one frame unless the connection has already been stopped.
/// The original watch receiver makes a stop sent before the first poll visible.
pub(super) async fn send_frame_or_stop(
    media_sender: &Arc<dyn MediaSender>,
    error_tx: &mpsc::UnboundedSender<WorkerToService>,
    connection_id: &str,
    frame: MediaFrame,
    stop_rx: &mut watch::Receiver<bool>,
) -> bool {
    if *stop_rx.borrow() {
        return false;
    }
    tokio::select! {
        biased;
        _ = stop_rx.changed() => false,
        sent = send_frame(media_sender, error_tx, connection_id, frame) => sent,
    }
}
