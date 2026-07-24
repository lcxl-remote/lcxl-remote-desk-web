//! Data-channel error classification and SDP size limits.

use super::*;

/// Categorise a `webrtc::Error` from `dc.send` / `dc.send_text` into
/// the variants the worker reacts to. The webrtc-rs error chain is
/// `webrtc::Error::Sctp(webrtc_sctp::Error::ErrOutboundPacketTooLarge)`
/// for the "chunk too large" case; rather than reaching into the
/// nested error type (the `Sctp` arm is private to webrtc-rs and
/// could be refactored), match on the rendered `Display` substring.
/// The substring `"OutboundPacketTooLarge"` is stable across
/// webrtc-rs 0.17.x and uniquely identifies the SCTP wire-level
/// rejection that the 256 KiB chunk-size regression hit in
/// 2026-05-11.
pub(super) fn classify_dc_send_error(err: &webrtc::Error) -> FileTransferSendErrorKind {
    let rendered = err.to_string();
    if rendered.contains("OutboundPacketTooLarge") {
        FileTransferSendErrorKind::PacketTooLarge
    } else if rendered.contains("closed")
        || rendered.contains("Closed")
        || rendered.contains("StreamClosed")
        || rendered.contains("ConnectionClosed")
    {
        FileTransferSendErrorKind::TransportClosed
    } else {
        FileTransferSendErrorKind::Other
    }
}

/// Parse `a=max-message-size:N` out of a remote SDP. Returns `None`
/// when the attribute is absent (some browsers / older versions skip
/// it, in which case the SCTP RFC 8841 default of 65536 applies — but
/// we don't synthesise that here; the caller logs the gap).
///
/// The attribute can appear on the session level or under the
/// `m=application` (DataChannel) media section; either case wins. We
/// match the literal `a=max-message-size:` prefix because there is no
/// other SDP attribute that shares the prefix, and we deliberately
/// don't bring in a full SDP parser for one line — keeping the
/// dependency surface minimal.
pub(super) fn parse_sdp_max_message_size(sdp: &str) -> Option<u64> {
    const PREFIX: &str = "a=max-message-size:";
    sdp.lines().find_map(|line| {
        let trimmed = line.trim();
        trimmed.strip_prefix(PREFIX).and_then(|rest| {
            rest.split_whitespace()
                .next()
                .and_then(|n| n.parse::<u64>().ok())
        })
    })
}

/// Log the offer's `a=max-message-size` advertise and assert that our
/// chosen file-transfer chunk size fits inside it (chunk + 40-byte
/// binary header).
///
/// `info!` on success: useful when correlating production logs
/// against a chunk-size regression — knowing the actual negotiated
/// value retroactively explains a `PacketTooLarge` from
/// [`classify_dc_send_error`].
///
/// `error!` on violation: the SCTP send will reject the very first
/// binary chunk with `ErrOutboundPacketTooLarge`. Surfacing it at
/// offer time gives operators a chance to roll back the chunk-size
/// change before the next download starts, instead of finding out
/// only when the first file fails.
///
/// `warn!` when the attribute is absent: per RFC 8841 §6 the default
/// is 65536 bytes (64 KiB), which is **smaller** than our 240 KiB +
/// 40 B header. A peer that doesn't advertise the attribute is on
/// some old WebRTC stack that probably also doesn't lift the default,
/// so we proactively warn.
pub(super) fn log_sdp_max_message_size(connection_id: &str, sdp: &str) {
    // Constants from the worker dispatcher reach across the
    // daemon ↔ worker boundary because chunk_size is currently a
    // compile-time constant on the worker side. Future chunk-size
    // negotiation would consult this value to pick the maximum; for now
    // we just check our static choice fits.
    use crate::model::file_transfer::BINARY_HEADER_SIZE;
    use crate::worker::file_transfer_dispatcher::FILE_TRANSFER_CHUNK_SIZE_TX;
    let required = (FILE_TRANSFER_CHUNK_SIZE_TX + BINARY_HEADER_SIZE) as u64;
    match parse_sdp_max_message_size(sdp) {
        Some(advertised) => {
            if advertised < required {
                log::error!(
                    "[pc_manager] {connection_id}: remote SDP advertises \
                     max-message-size={advertised} but our chunk \
                     (FILE_TRANSFER_CHUNK_SIZE_TX={FILE_TRANSFER_CHUNK_SIZE_TX} + \
                     BINARY_HEADER_SIZE={BINARY_HEADER_SIZE} = {required} B) won't fit; \
                     downloads will fail with ErrOutboundPacketTooLarge — \
                     lower FILE_TRANSFER_CHUNK_SIZE_TX or use a browser that advertises a \
                     larger ceiling"
                );
            } else {
                log::info!(
                    "[pc_manager] {connection_id}: remote SDP max-message-size={advertised} \
                     (chunk+header={required})"
                );
            }
        }
        None => {
            log::warn!(
                "[pc_manager] {connection_id}: remote SDP has no a=max-message-size; \
                 falling back to RFC 8841 default 65536 which is below our chunk+header \
                 ({required} B). Downloads to this peer may fail with \
                 ErrOutboundPacketTooLarge"
            );
        }
    }
}
