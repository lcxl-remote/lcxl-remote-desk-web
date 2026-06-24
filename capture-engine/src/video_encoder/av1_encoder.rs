use std::{sync::Arc, sync::LazyLock, time::Instant};

use desk_signal_facade::model::{desk_settings::Av1EncoderSettings, image_capture::DisplayInfo};
use prometheus::{HistogramVec, register_histogram_vec};
use rav1e::prelude::*;

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

pub struct Av1Encoder {
    pub ctx: Context<u8>,
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

        let mut enc = EncoderConfig::with_speed_preset(setting.speed as u8);
        enc.width = width;
        enc.height = height;
        enc.chroma_sampling = ChromaSampling::Cs420;
        enc.quantizer = setting.quality as usize;
        enc.min_key_frame_interval = 12;
        enc.max_key_frame_interval = 240;
        enc.low_latency = true;
        enc.bitrate = 0; // 0 = use quantizer mode (CQ)

        // Real-time remote desktop produces frames on demand. The speed preset
        // leaves `rdo_lookahead_frames` at 10, so rav1e holds back a deep window
        // before it emits a packet (see `needs_more_fi_lookahead`). When the
        // screen changes slowly that buffered tail stalls output for several
        // seconds. rav1e requires this be >= 1, so pin it to the minimum to keep
        // the emit latency as short as the codec allows.
        enc.speed_settings.rdo_lookahead_frames = 1;

        // Unlike libvpx / x264, rav1e is a pure-Rust software encoder with no
        // built-in threading unless asked. `with_threads(0)` creates no pool and
        // a single tile, so a 1080p frame is encoded serially on one core. Split
        // the frame into tiles and give rav1e a matching thread pool so tiles
        // encode in parallel.
        let threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        enc.tiles = threads;

        let cfg = Config::new().with_encoder_config(enc).with_threads(threads);
        let ctx: Context<u8> = cfg.new_context().map_err(|e| {
            CaptureError::AnyhowError(anyhow::anyhow!("rav1e context creation failed: {:?}", e))
        })?;

        Ok(Self {
            ctx,
            width: width as u32,
            height: height as u32,
            yuv_buffer: None,
        })
    }
}

impl Av1Encoder {
    fn encode_with_ctx(
        ctx: &mut Context<u8>,
        encode_timer: Instant,
        yuv: &PersistentYuvBuffer,
    ) -> Result<Vec<NalInfo>, CaptureError> {
        let mut frame = ctx.new_frame();
        frame.planes[0].copy_from_raw_u8(yuv.y_plane(), yuv.y_stride as usize, 1);
        frame.planes[1].copy_from_raw_u8(yuv.u_plane(), yuv.u_stride as usize, 1);
        frame.planes[2].copy_from_raw_u8(yuv.v_plane(), yuv.v_stride as usize, 1);

        ctx.send_frame(Arc::new(frame)).map_err(|e| {
            CaptureError::AnyhowError(anyhow::anyhow!("rav1e send_frame failed: {:?}", e))
        })?;

        let mut nal_infos = Vec::new();
        loop {
            match ctx.receive_packet() {
                Ok(packet) => {
                    let is_keyframe = matches!(packet.frame_type, FrameType::KEY);
                    let frame_type_str = if is_keyframe { "key" } else { "inter" };
                    ENCODE_TO_AV1_HISTOGRAM
                        .with_label_values(&[frame_type_str])
                        .observe(duration_to_seconds(
                            Instant::now().saturating_duration_since(encode_timer),
                        ));
                    nal_infos.push(NalInfo {
                        nal_bytes: bytes::Bytes::from(packet.data),
                        is_keyframe,
                    });
                }
                Err(EncoderStatus::NeedMoreData) => break,
                Err(EncoderStatus::Encoded) => continue,
                Err(e) => {
                    return Err(CaptureError::AnyhowError(anyhow::anyhow!(
                        "rav1e receive_packet failed: {:?}",
                        e
                    )));
                }
            }
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
            &mut self.ctx,
            Instant::now(),
            self.yuv_buffer.as_ref().unwrap(),
        )
    }

    fn encode_cached(&mut self) -> Result<Vec<NalInfo>, CaptureError> {
        let Some(_) = self.yuv_buffer.as_ref() else {
            return Ok(vec![]);
        };
        Av1Encoder::encode_with_ctx(
            &mut self.ctx,
            Instant::now(),
            self.yuv_buffer.as_ref().unwrap(),
        )
    }

    fn request_keyframe(&mut self) {
        // Fallback for AV1 since we recreate the encoder in signaling.rs
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

    /// Regression: the speed-10 preset sets `rdo_lookahead_frames = 10`, which
    /// makes rav1e hold back roughly a dozen frames before it emits the first
    /// packet (empirically the 13th frame). On a remote desktop that produces
    /// frames on demand, that buffered tail shows up as multi-second latency.
    /// Pinning the lookahead to its minimum (1) shrinks the warm-up to a handful
    /// of frames (empirically the 5th). This test feeds identical frames one at
    /// a time and asserts a packet appears within the first five — which would
    /// not hold under the default preset's deep lookahead.
    #[test]
    fn emits_packet_within_a_few_frames_low_latency_lookahead() {
        let setting = Av1EncoderSettings {
            quality: 100,
            speed: 10,
        };
        let mut encoder =
            Av1Encoder::new(setting, &display_info(128, 128)).expect("create av1 encoder");

        let frame = StubBgraImage::new(128, 128);
        let mut total = 0usize;
        let mut saw_keyframe = false;
        for _ in 0..5 {
            let nals = encoder.encode(&frame, false).expect("encode frame");
            saw_keyframe |= nals.iter().any(|n| n.is_keyframe);
            total += nals.len();
        }

        assert!(
            total > 0,
            "a packet must be emitted within five frames; the default preset's \
             deep lookahead would have stalled output well past this point"
        );
        assert!(saw_keyframe, "the first emitted packet must be a keyframe");
    }
}
