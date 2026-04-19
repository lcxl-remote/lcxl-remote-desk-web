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
    video_encoder::{
        encoder_utils::duration_to_seconds, yuv_utils::convert_image_to_yuv420,
    },
};

pub static ENCODE_TO_AV1_HISTOGRAM: LazyLock<HistogramVec> = LazyLock::new(|| {
    register_histogram_vec!("encode_to_av1_histogram", "help", &["frame_type"]).unwrap()
});

pub struct Av1Encoder {
    pub ctx: Context<u8>,
    pub width: u32,
    pub height: u32,
}

impl Av1Encoder {
    pub fn new(setting: Av1EncoderSettings, display_info: &DisplayInfo) -> Result<Self, CaptureError> {
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

        let cfg = Config::new().with_encoder_config(enc).with_threads(0);
        let ctx: Context<u8> = cfg.new_context().map_err(|e| {
            CaptureError::AnyhowError(anyhow::anyhow!("rav1e context creation failed: {:?}", e))
        })?;

        Ok(Self {
            ctx,
            width: width as u32,
            height: height as u32,
        })
    }
}

impl VideoEncoder for Av1Encoder {
    fn encode(&mut self, image_info: &dyn ImageInfo) -> Result<Vec<NalInfo>, CaptureError> {
        let planar_image = convert_image_to_yuv420(image_info)?;
        let encode_timer = Instant::now();

        // Build rav1e Frame and copy YUV data into it
        let mut frame = self.ctx.new_frame();

        let y = planar_image.y_plane.borrow();
        let u = planar_image.u_plane.borrow();
        let v = planar_image.v_plane.borrow();

        // Use copy_from_raw_u8 to import YUV data into Frame.
        // The third argument (1) is val_size: each pixel channel is 1 byte (8-bit).
        frame.planes[0].copy_from_raw_u8(&y, planar_image.y_stride as usize, 1);
        frame.planes[1].copy_from_raw_u8(&u, planar_image.u_stride as usize, 1);
        frame.planes[2].copy_from_raw_u8(&v, planar_image.v_stride as usize, 1);

        // Send frame to encoder
        self.ctx.send_frame(Arc::new(frame)).map_err(|e| {
            CaptureError::AnyhowError(anyhow::anyhow!("rav1e send_frame failed: {:?}", e))
        })?;

        // Receive encoded packets
        let mut nal_infos = Vec::new();
        loop {
            match self.ctx.receive_packet() {
                Ok(packet) => {
                    let frame_type_str = match packet.frame_type {
                        FrameType::KEY => "key",
                        _ => "inter",
                    };
                    ENCODE_TO_AV1_HISTOGRAM
                        .with_label_values(&[frame_type_str])
                        .observe(duration_to_seconds(
                            Instant::now().saturating_duration_since(encode_timer),
                        ));
                    nal_infos.push(NalInfo {
                        nal_bytes: bytes::Bytes::from(packet.data),
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

    fn request_keyframe(&mut self) {
        // Fallback for AV1 since we recreate the encoder in signaling.rs
    }
}
