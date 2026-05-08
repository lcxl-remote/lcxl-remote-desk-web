use std::{sync::LazyLock, time::Instant};

use desk_signal_facade::model::{desk_settings::X264EncoderSettings, image_capture::DisplayInfo};
use prometheus::{HistogramVec, register_histogram_vec};
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

        let encoder = setup
            .build(Colorspace::I420, width, height)
            .map_err(|_| CaptureError::AnyhowError(anyhow::anyhow!("x264 build failed")))?;

        Ok(Self {
            encoder,
            pts: 0,
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
    fn encode(&mut self, image_info: &dyn ImageInfo) -> Result<Vec<NalInfo>, CaptureError> {
        if self.yuv_buffer.is_none() {
            self.yuv_buffer = Some(PersistentYuvBuffer::new(
                image_info.get_width(),
                image_info.get_height(),
            ));
        }
        self.yuv_buffer.as_mut().unwrap().update(image_info)?;
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
}
