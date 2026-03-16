use std::{sync::LazyLock, time::Instant};

use desk_signal_facade::model::{desk_settings::X264EncoderSettings, image_capture::DisplayInfo};
use prometheus::{HistogramVec, register_histogram_vec};
use x264::{Colorspace, Encoder, Image, Plane, Preset, Setup, Tune};

use crate::{
    error::DeskError,
    model::{
        image_capture::ImageInfo,
        video_encoder::{NalInfo, VideoEncoder},
    },
    service::video_encoder::{
        encoder_utils::duration_to_seconds, yuv_utils::convert_image_to_yuv420,
    },
};

pub struct X264Encoder {
    pub encoder: Encoder,
    pub pts: i64,
}

pub static ENCODE_TO_X264_HISTOGRAM: LazyLock<HistogramVec> = LazyLock::new(|| {
    register_histogram_vec!("encode_to_x264_histogram", "help", &["frame_type"]).unwrap()
});

impl X264Encoder {
    pub fn new(
        setting: X264EncoderSettings,
        display_info: &DisplayInfo,
    ) -> Result<Self, DeskError> {
        let width = display_info.desktop_coordinates.width() as i32;
        let height = display_info.desktop_coordinates.height() as i32;

        // Use zerolatency by passing zero_latency=true to Setup::preset
        let mut setup = Setup::preset(Preset::Ultrafast, Tune::None, false, true);

        // Use CRF for constant quality mode
        setup = setup.crf(setting.quality as f32);
        setup = setup.fps(60, 1);

        let encoder = setup
            .build(Colorspace::I420, width, height)
            .map_err(|_| DeskError::AnyhowError(anyhow::anyhow!("x264 build failed")))?;

        Ok(Self { encoder, pts: 0 })
    }
}

impl VideoEncoder for X264Encoder {
    fn encode(&mut self, image_info: &dyn ImageInfo) -> Result<Vec<NalInfo>, DeskError> {
        let planar_image = convert_image_to_yuv420(image_info)?;

        let encode_to_h264_timer = Instant::now();

        let y = planar_image.y_plane.borrow();
        let u = planar_image.u_plane.borrow();
        let v = planar_image.v_plane.borrow();

        let width = image_info.get_width() as i32;
        let height = image_info.get_height() as i32;

        let planes = [
            Plane {
                stride: width,
                data: &y,
            },
            Plane {
                stride: width / 2,
                data: &u,
            },
            Plane {
                stride: width / 2,
                data: &v,
            },
        ];

        let image = Image::new(Colorspace::I420, width, height, &planes);

        let (res, _out_picture) = self.encoder.encode(self.pts, image).map_err(|e| {
            DeskError::AnyhowError(anyhow::anyhow!("x264 encode error: {:?}", e))
        })?;

        self.pts += 1;

        let mut nal_infos = Vec::new();

        // Use .entirety() to get the encoded byte slice from x264::Data
        let data = bytes::Bytes::copy_from_slice(res.entirety());

        nal_infos.push(NalInfo { nal_bytes: data });

        ENCODE_TO_X264_HISTOGRAM
            .with_label_values(&["encoded"])
            .observe(duration_to_seconds(
                Instant::now().saturating_duration_since(encode_to_h264_timer),
            ));

        Ok(nal_infos)
    }
}
