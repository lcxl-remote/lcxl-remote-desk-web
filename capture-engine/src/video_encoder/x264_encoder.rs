use std::{sync::LazyLock, time::Instant};

use desk_signal_facade::model::{
    desk_settings::{default_video_bps, X264EncoderSettings},
    image_capture::DisplayInfo,
};
use prometheus::{register_histogram_vec, HistogramVec};
use x264::{Colorspace, Encoder, Image, Plane, Preset, Setup, Tune};

use crate::{
    error::CaptureError,
    model::{
        image_capture::ImageInfo,
        video_encoder::{NalInfo, VideoEncoder},
    },
    video_encoder::{encoder_utils::duration_to_seconds, yuv_utils::PersistentYuvBuffer},
};

pub struct X264Encoder {
    pub encoder: Encoder,
    pub pts: i64,
    /// VBV ceiling (kbps) the encoder was built with. x264 can only
    /// adjust an already-enabled VBV at runtime, so `new` always
    /// enables a loose ceiling and `set_bitrate_cap(None)` restores
    /// this value.
    initial_vbv_kbps: i32,
    yuv_buffer: Option<PersistentYuvBuffer>,
}

pub static ENCODE_TO_X264_HISTOGRAM: LazyLock<HistogramVec> = LazyLock::new(|| {
    register_histogram_vec!("encode_to_x264_histogram", "help", &["frame_type"]).unwrap()
});

impl X264Encoder {
    pub fn new(
        setting: X264EncoderSettings,
        display_info: &DisplayInfo,
        fps: u32,
    ) -> Result<Self, CaptureError> {
        let width = display_info.desktop_coordinates.width();
        let height = display_info.desktop_coordinates.height();

        let mut setup = Setup::preset(Preset::Ultrafast, Tune::None, false, true);
        setup = setup.crf(setting.quality as f32);
        setup = setup.fps(fps.max(1), 1);
        if setting.gop > 0 {
            setup = setup.keyint(setting.gop);
        }

        // Always enable VBV with the loosest sensible ceiling (the
        // quality-0 bits-per-pixel default for this resolution/fps) so
        // the encoder runs in constrained-quality mode: CRF drives the
        // steady state while VBV bounds bitrate spikes. Enabling it at
        // build time is mandatory — x264_encoder_reconfig can adjust
        // VBV values but cannot turn VBV on later.
        let initial_vbv_kbps =
            ((default_video_bps(width as u64, height as u64, fps.max(1) as u64, 0) / 1000).max(1))
                as i32;
        setup = setup.vbv(initial_vbv_kbps, initial_vbv_kbps);

        let encoder = setup
            .build(Colorspace::I420, width, height)
            .map_err(|_| CaptureError::AnyhowError(anyhow::anyhow!("x264 build failed")))?;

        Ok(Self {
            encoder,
            pts: 0,
            initial_vbv_kbps,
            yuv_buffer: None,
        })
    }

    fn encode_with_encoder(
        encoder: &mut Encoder,
        pts: &mut i64,
        yuv: &PersistentYuvBuffer,
    ) -> Result<Vec<NalInfo>, CaptureError> {
        let encode_timer = Instant::now();
        let width = yuv.width as i32;

        let planes = [
            Plane {
                stride: width,
                data: yuv.y_plane(),
            },
            Plane {
                stride: width / 2,
                data: yuv.u_plane(),
            },
            Plane {
                stride: width / 2,
                data: yuv.v_plane(),
            },
        ];

        let image = Image::new(Colorspace::I420, width, yuv.height as i32, &planes);
        let (res, out_picture) = encoder.encode(*pts, image).map_err(|e| {
            CaptureError::AnyhowError(anyhow::anyhow!("x264 encode error: {:?}", e))
        })?;
        *pts += 1;

        let data = bytes::Bytes::copy_from_slice(res.entirety());
        let is_keyframe = out_picture.keyframe();
        ENCODE_TO_X264_HISTOGRAM
            .with_label_values(&["encoded"])
            .observe(duration_to_seconds(
                Instant::now().saturating_duration_since(encode_timer),
            ));
        Ok(vec![NalInfo {
            nal_bytes: data,
            is_keyframe,
        }])
    }
}

impl VideoEncoder for X264Encoder {
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
        X264Encoder::encode_with_encoder(
            &mut self.encoder,
            &mut self.pts,
            self.yuv_buffer.as_ref().unwrap(),
        )
    }

    fn encode_cached(&mut self) -> Result<Vec<NalInfo>, CaptureError> {
        let Some(_) = self.yuv_buffer.as_ref() else {
            return Ok(vec![]);
        };
        X264Encoder::encode_with_encoder(
            &mut self.encoder,
            &mut self.pts,
            self.yuv_buffer.as_ref().unwrap(),
        )
    }

    fn set_bitrate_cap(&mut self, cap_kbps: Option<u32>) -> bool {
        // Never widen beyond the build-time ceiling: it is already the
        // loosest sensible value for this resolution/fps.
        let kbps = match cap_kbps {
            Some(k) => (k as i32).clamp(1, self.initial_vbv_kbps),
            None => self.initial_vbv_kbps,
        };
        match self.encoder.reconfig_vbv(kbps, kbps) {
            Ok(()) => true,
            Err(e) => {
                log::warn!("x264 reconfig_vbv({kbps} kbps) failed: {e:?}");
                false
            }
        }
    }
}
