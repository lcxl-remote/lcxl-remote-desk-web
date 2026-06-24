use std::{num::NonZeroUsize, sync::LazyLock, time::Instant};

use desk_signal_facade::model::{desk_settings::Av1EncoderSettings, image_capture::DisplayInfo};
use prometheus::{HistogramVec, register_histogram_vec};
use shiguredo_svt_av1::{ColorFormat, EncodeOptions, Encoder, EncoderConfig, FrameData, RcMode};

use crate::{
    error::CaptureError,
    model::{
        image_capture::ImageInfo,
        video_encoder::{NalInfo, VideoEncoder},
    },
    video_encoder::{encoder_utils::duration_to_seconds, yuv_utils::PersistentYuvBuffer},
};

pub static ENCODE_TO_AV1_HISTOGRAM: LazyLock<HistogramVec> = LazyLock::new(|| {
    register_histogram_vec!("encode_to_av1_histogram", "help", &["frame_type"]).unwrap()
});

/// SVT-AV1 preset range: 0 (best quality) .. 13 (fastest).
const SVT_PRESET_MAX: u32 = 13;
/// SVT-AV1 CRF/QP range: 0 (best quality) .. 63 (worst).
const SVT_CRF_MAX: u32 = 63;
/// Periodic keyframe interval. WebRTC recovers from loss via PLI/FIR (which
/// rebuilds the encoder), but a bounded intra period still bounds how long a
/// late joiner / recovered stream waits for a decodable point.
const INTRA_PERIOD_FRAMES: usize = 240;

pub struct Av1Encoder {
    enc: Encoder,
    pub width: u32,
    pub height: u32,
    yuv_buffer: Option<PersistentYuvBuffer>,
}

impl Av1Encoder {
    pub fn new(
        setting: Av1EncoderSettings,
        display_info: &DisplayInfo,
    ) -> Result<Self, CaptureError> {
        let width = display_info.desktop_coordinates.width() as usize;
        let height = display_info.desktop_coordinates.height() as usize;

        let mut config = EncoderConfig::new(width, height, ColorFormat::I420);

        // CRF / constant-quality mode: the quantizer (`qp`) drives quality and
        // there is no target bitrate to satisfy. This mirrors the previous
        // rav1e quantizer-mode behaviour.
        config.rate_control_mode = RcMode::CqpOrCrf;
        config.qp = Some(setting.crf.min(SVT_CRF_MAX) as u8);
        // CRF / constant-quality mode rejects a non-zero target bitrate (the
        // constructor defaults it to a VBR value), so clear it.
        config.target_bit_rate = 0;

        // Preset: higher = faster. Remote desktop prioritises latency/throughput
        // over compression, so the default leans to the fast end.
        config.enc_mode = setting.preset.min(SVT_PRESET_MAX) as u8;

        // Real-time low-delay mode: this is the core fix over rav1e. With RTC on,
        // SVT-AV1 emits a packet per input frame instead of buffering a deep
        // look-ahead window, so a remote desktop that produces frames on demand
        // does not stall for several seconds before the first packet.
        config.rtc = Some(setting.rtc);

        config.intra_period_length = NonZeroUsize::new(INTRA_PERIOD_FRAMES);

        let enc = Encoder::new(config).map_err(|e| {
            CaptureError::AnyhowError(anyhow::anyhow!("SVT-AV1 encoder creation failed: {:?}", e))
        })?;

        Ok(Self {
            enc,
            width: width as u32,
            height: height as u32,
            yuv_buffer: None,
        })
    }
}

impl Drop for Av1Encoder {
    fn drop(&mut self) {
        // SVT-AV1 expects an end-of-stream signal before the underlying handle
        // is deinitialised; without it the library logs "deinit called without
        // sending EOS!". The worker rebuilds this encoder on PLI/FIR and on
        // resolution changes, so teardown is a normal, frequent event. Any
        // frames still buffered are intentionally discarded — the stream is
        // over from this encoder's perspective.
        let _ = self.enc.finish();
    }
}

impl Av1Encoder {
    fn encode_with_ctx(
        enc: &mut Encoder,
        encode_timer: Instant,
        yuv: &PersistentYuvBuffer,
    ) -> Result<Vec<NalInfo>, CaptureError> {
        // `PersistentYuvBuffer` lays planes out tightly (`y_stride = width`,
        // chroma stride = `ceil(width / 2)`), which is exactly the packing
        // SVT-AV1 expects for its `width` / `ceil(width / 2)` strides. The plane
        // slices can therefore be handed over directly — no stride argument
        // (unlike rav1e's `copy_from_raw_u8(.., stride, ..)`).
        let frame = FrameData::I420 {
            y: yuv.y_plane(),
            u: yuv.u_plane(),
            v: yuv.v_plane(),
        };
        enc.encode(&frame, &EncodeOptions::default()).map_err(|e| {
            CaptureError::AnyhowError(anyhow::anyhow!("SVT-AV1 encode failed: {:?}", e))
        })?;

        let mut nal_infos = Vec::new();
        while let Some(packet) = enc.next_frame() {
            let is_keyframe = packet.is_keyframe();
            let frame_type_str = if is_keyframe { "key" } else { "inter" };
            ENCODE_TO_AV1_HISTOGRAM
                .with_label_values(&[frame_type_str])
                .observe(duration_to_seconds(
                    Instant::now().saturating_duration_since(encode_timer),
                ));
            nal_infos.push(NalInfo {
                nal_bytes: bytes::Bytes::copy_from_slice(packet.data()),
                is_keyframe,
            });
        }
        log::trace!("Encoded to AV1 format, {} packets", nal_infos.len());
        Ok(nal_infos)
    }
}

impl VideoEncoder for Av1Encoder {
    fn encode(
        &mut self,
        image_info: &dyn ImageInfo,
        enable_dirty_rect: bool,
    ) -> Result<Vec<NalInfo>, CaptureError> {
        if self.yuv_buffer.is_none() {
            self.yuv_buffer = Some(PersistentYuvBuffer::new(
                image_info.get_width(),
                image_info.get_height(),
            ));
        }
        self.yuv_buffer
            .as_mut()
            .unwrap()
            .update(image_info, enable_dirty_rect)?;
        Av1Encoder::encode_with_ctx(
            &mut self.enc,
            Instant::now(),
            self.yuv_buffer.as_ref().unwrap(),
        )
    }

    fn encode_cached(&mut self) -> Result<Vec<NalInfo>, CaptureError> {
        let Some(_) = self.yuv_buffer.as_ref() else {
            return Ok(vec![]);
        };
        Av1Encoder::encode_with_ctx(
            &mut self.enc,
            Instant::now(),
            self.yuv_buffer.as_ref().unwrap(),
        )
    }

    fn request_keyframe(&mut self) {
        // No-op: keyframe recovery for AV1 goes through the worker rebuilding
        // the encoder when the daemon relays a browser PLI/FIR (see
        // `media_producer`), which produces a fresh IDR. SVT-AV1 also supports
        // per-frame forced keyframes (`EncodeOptions::force_keyframe`) as a
        // future optimisation to avoid the rebuild.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::image_capture::{ImageInfo, ImageType};
    use desk_signal_facade::model::image_capture::{DisplayInfo, DisplayRect};

    struct StubBgraImage {
        width: u32,
        height: u32,
        data: Vec<u8>,
    }

    impl StubBgraImage {
        fn new(width: u32, height: u32) -> Self {
            Self {
                width,
                height,
                data: vec![0x80u8; (width as usize) * (height as usize) * 4],
            }
        }
    }

    impl ImageInfo for StubBgraImage {
        fn get_type(&self) -> ImageType {
            ImageType::BGRA
        }
        fn get_data(&self) -> &[u8] {
            &self.data
        }
        fn get_width(&self) -> u32 {
            self.width
        }
        fn get_height(&self) -> u32 {
            self.height
        }
    }

    fn display_info(width: i32, height: i32) -> DisplayInfo {
        DisplayInfo {
            desktop_coordinates: DisplayRect {
                left: 0,
                top: 0,
                right: width,
                bottom: height,
            },
            ..Default::default()
        }
    }

    fn default_setting() -> Av1EncoderSettings {
        Av1EncoderSettings::default()
    }

    /// Latency regression: rav1e held back ~13 frames before emitting the first
    /// packet because of its deep `rdo_lookahead_frames` window, which on an
    /// on-demand remote desktop showed up as multi-second stalls. SVT-AV1 in RTC
    /// mode must not reintroduce that warm-up — feeding a handful of identical
    /// frames has to yield at least one packet, and a keyframe first.
    #[test]
    fn emits_packet_within_a_few_frames_in_rtc_mode() {
        let mut encoder = Av1Encoder::new(default_setting(), &display_info(256, 256))
            .expect("create av1 encoder");

        let frame = StubBgraImage::new(256, 256);
        let mut total = 0usize;
        let mut saw_keyframe = false;
        for _ in 0..5 {
            let nals = encoder.encode(&frame, false).expect("encode frame");
            saw_keyframe |= nals.iter().any(|n| n.is_keyframe);
            total += nals.len();
        }

        assert!(
            total > 0,
            "RTC mode must emit a packet within a few frames; a deep look-ahead \
             window would have stalled output well past this point"
        );
        assert!(saw_keyframe, "an emitted packet must be a keyframe");
    }

    /// The very first emitted packet must be a keyframe so a freshly connected
    /// decoder can start without waiting for the periodic intra period.
    #[test]
    fn first_emitted_packet_is_keyframe() {
        let mut encoder = Av1Encoder::new(default_setting(), &display_info(256, 256))
            .expect("create av1 encoder");
        let frame = StubBgraImage::new(256, 256);

        let mut first: Option<bool> = None;
        for _ in 0..5 {
            let nals = encoder.encode(&frame, false).expect("encode frame");
            if let Some(n) = nals.first() {
                first = Some(n.is_keyframe);
                break;
            }
        }
        assert_eq!(
            first,
            Some(true),
            "the first AV1 packet emitted must be a keyframe"
        );
    }

    /// OBU compatibility: the daemon hands the encoder output straight to
    /// webrtc-rs, whose `Av1Payloader` parses the bytes as low-overhead OBUs
    /// (header + LEB128 size) rather than blindly fragmenting them. This proves
    /// SVT-AV1's real output is accepted by that payloader, which is the actual
    /// wire contract — not an inferred one.
    #[test]
    fn output_is_parseable_by_webrtc_av1_payloader() {
        use rtp::packetizer::Payloader;

        let mut encoder = Av1Encoder::new(default_setting(), &display_info(256, 256))
            .expect("create av1 encoder");
        let frame = StubBgraImage::new(256, 256);

        let mut packet: Option<bytes::Bytes> = None;
        for _ in 0..5 {
            let nals = encoder.encode(&frame, false).expect("encode frame");
            if let Some(n) = nals.into_iter().next() {
                packet = Some(n.nal_bytes);
                break;
            }
        }
        let payload = packet.expect("encoder produced at least one packet");

        let mut payloader = rtp::codecs::av1::Av1Payloader::default();
        let fragments = payloader
            .payload(1200, &payload)
            .expect("SVT-AV1 output must be parseable as AV1 OBUs by the webrtc payloader");
        assert!(
            !fragments.is_empty(),
            "payloader must produce at least one RTP payload fragment"
        );
    }
}
