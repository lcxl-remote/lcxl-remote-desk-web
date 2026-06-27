//! # Server-side video codec negotiation
//!
//! The daemon encodes a *single* video codec per PeerConnection and puts
//! it on a [`TrackLocalStaticSample`]. Historically that codec was taken
//! verbatim from `offer.desk_settings.video_encoder` — a value the
//! controller (browser / Android / iOS) asserts unilaterally. When the
//! asserted codec is not actually decodable by that client, the handshake
//! either fails (`codec is not supported by remote`) or the client renders
//! a black screen.
//!
//! This module derives the codec from the *negotiated* intersection
//! instead: the codecs the client advertises in its offer's `m=video`
//! `a=rtpmap` lines (its real decode capability, produced by each
//! platform's native WebRTC `createOffer`) intersected with the codecs the
//! host can actually encode. `offer.desk_settings.video_encoder` is demoted
//! to a *preference hint*: honoured when the client can decode it, ignored
//! otherwise. The result works uniformly for every client without any
//! client-side capability probing.
//!
//! All functions here are pure (the SDP text and codec slices are the only
//! inputs), so the negotiation logic is exercised entirely by unit tests
//! without a live PeerConnection.

use std::io::Cursor;

use desk_capture_engine::video_encoder::video_encoder_factory::list_video_encoder;
use desk_ipc_protocol::message::MediaCodec;
use webrtc::sdp::description::session::SessionDescription;

/// Map a codec identifier — either an `a=rtpmap` codec name (`VP8`, `VP9`,
/// `H264`, `AV1`) or a capture-engine encoder name (`X264`, `H264`, …) — to
/// its wire [`MediaCodec`]. `X264` and `H264` both produce an H.264
/// bitstream, so they collapse onto [`MediaCodec::H264`], matching the
/// daemon→worker contract (which is wire-codec, not encoder-impl, granular).
/// Returns `None` for anything else (RTX / RED / ULPFEC / FEC / audio).
fn name_to_media_codec(name: &str) -> Option<MediaCodec> {
    match name.to_ascii_uppercase().as_str() {
        "VP8" => Some(MediaCodec::Vp8),
        "VP9" => Some(MediaCodec::Vp9),
        "H264" | "X264" => Some(MediaCodec::H264),
        "AV1" => Some(MediaCodec::Av1),
        _ => None,
    }
}

/// Extract the video codecs a remote offer advertises it can decode, in the
/// order they appear in the first `m=video` section's `a=rtpmap` lines
/// (which mirrors the client's own codec preference). Duplicates and
/// non-video payloads (RTX / RED / ULPFEC) are dropped; order is preserved.
///
/// A malformed / unparseable SDP yields an empty vec, which the caller
/// treats as "no overlap" and falls back to the configured default codec.
pub fn parse_offer_video_codecs(sdp: &str) -> Vec<MediaCodec> {
    let mut reader = Cursor::new(sdp.as_bytes());
    let parsed = match SessionDescription::unmarshal(&mut reader) {
        Ok(p) => p,
        Err(_) => return Vec::new(),
    };

    let mut out: Vec<MediaCodec> = Vec::new();
    for media in &parsed.media_descriptions {
        if !media.media_name.media.eq_ignore_ascii_case("video") {
            continue;
        }
        for attr in &media.attributes {
            if !attr.key.eq_ignore_ascii_case("rtpmap") {
                continue;
            }
            // `a=rtpmap:<payload> <name>/<clock>[/<channels>]`
            // e.g. `96 VP8/90000`. The payload number is stripped by the
            // sdp crate? No — it is kept in `value`, so split it off here.
            let Some(value) = attr.value.as_deref() else {
                continue;
            };
            let Some(encoding) = value.split_whitespace().nth(1) else {
                continue;
            };
            let codec_name = encoding.split('/').next().unwrap_or(encoding);
            if let Some(codec) = name_to_media_codec(codec_name)
                && !out.contains(&codec)
            {
                out.push(codec);
            }
        }
    }
    out
}

/// The set of video wire codecs the host can encode, derived from the
/// capture-engine factory enumeration. `X264` and `H264` both collapse onto
/// [`MediaCodec::H264`]; `AV1` is present only where SVT-AV1 is compiled in
/// (the factory already omits it otherwise). Order follows the factory list;
/// duplicates are removed.
pub fn server_encodable_video_codecs() -> Vec<MediaCodec> {
    let mut out: Vec<MediaCodec> = Vec::new();
    for name in list_video_encoder() {
        if let Some(codec) = name_to_media_codec(&name)
            && !out.contains(&codec)
        {
            out.push(codec);
        }
    }
    out
}

/// Pick the codec the host should encode for a connection.
///
/// 1. If the `preferred` hint (from `desk_settings.video_encoder`) is in
///    the mutually-supported set, honour it — this preserves both the
///    host's configured default and a client's explicit override.
/// 2. Otherwise take the first codec in the client's offer order that the
///    host can encode, respecting each platform's native SDP preference.
/// 3. `None` when client and host share no codec at all (effectively
///    impossible — VP8 is a universal baseline — so the caller logs a
///    warning and falls back to the configured default).
pub fn negotiate_video_codec(
    client_decodable: &[MediaCodec],
    server_encodable: &[MediaCodec],
    preferred: Option<MediaCodec>,
) -> Option<MediaCodec> {
    let mutually = |c: MediaCodec| client_decodable.contains(&c) && server_encodable.contains(&c);

    if let Some(p) = preferred
        && mutually(p)
    {
        return Some(p);
    }

    client_decodable
        .iter()
        .copied()
        .find(|c| server_encodable.contains(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal but valid offer SDP with one `m=video` section whose
    /// `a=rtpmap` lines list `codecs` in order. Each codec gets a synthetic
    /// payload number starting at 96.
    fn video_offer(codecs: &[&str]) -> String {
        let mut sdp = String::from(
            "v=0\r\n\
             o=- 0 0 IN IP4 127.0.0.1\r\n\
             s=-\r\n\
             t=0 0\r\n",
        );
        let formats: Vec<String> = (0..codecs.len()).map(|i| (96 + i).to_string()).collect();
        sdp.push_str(&format!(
            "m=video 9 UDP/TLS/RTP/SAVPF {}\r\n",
            formats.join(" ")
        ));
        for (i, name) in codecs.iter().enumerate() {
            sdp.push_str(&format!("a=rtpmap:{} {}/90000\r\n", 96 + i, name));
        }
        sdp
    }

    #[test]
    fn parses_full_browser_codec_set_in_order() {
        let sdp = video_offer(&["VP8", "VP9", "H264", "AV1"]);
        assert_eq!(
            parse_offer_video_codecs(&sdp),
            vec![
                MediaCodec::Vp8,
                MediaCodec::Vp9,
                MediaCodec::H264,
                MediaCodec::Av1
            ]
        );
    }

    #[test]
    fn parses_emulator_set_without_h264() {
        // Android emulator / libwebrtc software decoders cover VP8/VP9/AV1
        // but not H264 — the offer simply omits the H264 rtpmap.
        let sdp = video_offer(&["VP8", "VP9", "AV1"]);
        let codecs = parse_offer_video_codecs(&sdp);
        assert!(!codecs.contains(&MediaCodec::H264));
        assert_eq!(
            codecs,
            vec![MediaCodec::Vp8, MediaCodec::Vp9, MediaCodec::Av1]
        );
    }

    #[test]
    fn parses_ios_h264_only() {
        let sdp = video_offer(&["H264"]);
        assert_eq!(parse_offer_video_codecs(&sdp), vec![MediaCodec::H264]);
    }

    #[test]
    fn ignores_non_video_payloads_and_dedups() {
        // RTX / RED / ULPFEC and a repeated VP8 must not leak in.
        let sdp = video_offer(&["VP8", "rtx", "red", "ulpfec", "VP8", "H264"]);
        assert_eq!(
            parse_offer_video_codecs(&sdp),
            vec![MediaCodec::Vp8, MediaCodec::H264]
        );
    }

    #[test]
    fn no_video_section_yields_empty() {
        let sdp = "v=0\r\n\
                   o=- 0 0 IN IP4 127.0.0.1\r\n\
                   s=-\r\n\
                   t=0 0\r\n\
                   m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n";
        assert!(parse_offer_video_codecs(sdp).is_empty());
    }

    #[test]
    fn malformed_sdp_yields_empty() {
        assert!(parse_offer_video_codecs("not an sdp").is_empty());
        assert!(parse_offer_video_codecs("").is_empty());
    }

    #[test]
    fn honours_preferred_when_mutually_supported() {
        let client = [MediaCodec::Vp8, MediaCodec::Vp9, MediaCodec::H264];
        let server = [MediaCodec::H264, MediaCodec::Vp8, MediaCodec::Vp9];
        assert_eq!(
            negotiate_video_codec(&client, &server, Some(MediaCodec::Vp9)),
            Some(MediaCodec::Vp9)
        );
    }

    #[test]
    fn falls_back_to_client_order_when_preferred_undecodable() {
        // Host prefers AV1 but this client can't decode it; pick the first
        // client-ordered codec the host can encode (VP8 here).
        let client = [MediaCodec::Vp8, MediaCodec::Vp9];
        let server = [MediaCodec::H264, MediaCodec::Vp8, MediaCodec::Vp9];
        assert_eq!(
            negotiate_video_codec(&client, &server, Some(MediaCodec::Av1)),
            Some(MediaCodec::Vp8)
        );
    }

    #[test]
    fn falls_back_to_client_order_when_no_preference() {
        let client = [MediaCodec::Vp9, MediaCodec::H264];
        let server = [MediaCodec::H264, MediaCodec::Vp9];
        assert_eq!(
            negotiate_video_codec(&client, &server, None),
            Some(MediaCodec::Vp9)
        );
    }

    #[test]
    fn preferred_not_encodable_by_server_falls_back() {
        // Client can decode H264, but the host cannot encode it (server set
        // lacks H264) — the preference must not win over encodability.
        let client = [MediaCodec::H264, MediaCodec::Vp8];
        let server = [MediaCodec::Vp8, MediaCodec::Vp9];
        assert_eq!(
            negotiate_video_codec(&client, &server, Some(MediaCodec::H264)),
            Some(MediaCodec::Vp8)
        );
    }

    #[test]
    fn empty_intersection_yields_none() {
        let client = [MediaCodec::Av1];
        let server = [MediaCodec::Vp8, MediaCodec::H264];
        assert_eq!(negotiate_video_codec(&client, &server, None), None);
        assert_eq!(
            negotiate_video_codec(&client, &server, Some(MediaCodec::Av1)),
            None
        );
    }

    #[test]
    fn server_set_has_baseline_and_only_video_wire_codecs() {
        // `av1_supported` is a capture-engine-local build cfg, so the server
        // crate cannot predict whether AV1 is present — only assert the
        // universal baseline and that every entry is a real video wire codec
        // (never Opus, and de-duplicated despite X264+H264 both mapping to
        // MediaCodec::H264).
        let server = server_encodable_video_codecs();
        assert!(server.contains(&MediaCodec::H264));
        assert!(server.contains(&MediaCodec::Vp8));
        assert!(server.contains(&MediaCodec::Vp9));
        assert!(!server.contains(&MediaCodec::Opus));
        let mut deduped = server.clone();
        deduped.dedup();
        assert_eq!(
            deduped.len(),
            server.len(),
            "server codec set must be unique"
        );
    }
}
