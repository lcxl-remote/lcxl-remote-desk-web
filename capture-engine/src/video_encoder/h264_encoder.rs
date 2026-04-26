use std::{sync::LazyLock, time::Instant};

use desk_signal_facade::model::desk_settings::H264EncoderSettings;
use openh264::{
    OpenH264API,
    encoder::{BitRate, IntraFramePeriod},
};
use prometheus::{HistogramVec, register_histogram_vec};
use yuv::YuvPlanarImageMut;

use crate::{
    error::CaptureError,
    model::{
        image_capture::ImageInfo,
        video_encoder::{NalInfo, VideoEncoder},
    },
    video_encoder::{encoder_utils::duration_to_seconds, yuv_utils::PersistentYuvBuffer},
};
use std::fmt::Debug;

#[derive(Debug)]
pub struct YuvPlanarImageWrapper<'a, T>
where
    T: Copy + Debug,
{
    pub inner: YuvPlanarImageMut<'a, T>,
}

impl<'a, T> YuvPlanarImageWrapper<'a, T>
where
    T: Copy + Debug,
{
    pub fn new(inner: YuvPlanarImageMut<'a, T>) -> Self {
        Self { inner }
    }
}

impl openh264::formats::YUVSource for YuvPlanarImageWrapper<'_, u8> {
    fn dimensions(&self) -> (usize, usize) {
        (self.inner.width as usize, self.inner.height as usize)
    }

    fn strides(&self) -> (usize, usize, usize) {
        (
            self.inner.y_stride as usize,
            self.inner.u_stride as usize,
            self.inner.v_stride as usize,
        )
    }

    fn y(&self) -> &[u8] {
        self.inner.y_plane.borrow()
    }

    fn u(&self) -> &[u8] {
        self.inner.u_plane.borrow()
    }

    fn v(&self) -> &[u8] {
        self.inner.v_plane.borrow()
    }
}

struct PersistentYuvView<'a>(&'a PersistentYuvBuffer);

impl openh264::formats::YUVSource for PersistentYuvView<'_> {
    fn dimensions(&self) -> (usize, usize) {
        (self.0.width as usize, self.0.height as usize)
    }

    fn strides(&self) -> (usize, usize, usize) {
        (
            self.0.y_stride as usize,
            self.0.u_stride as usize,
            self.0.v_stride as usize,
        )
    }

    fn y(&self) -> &[u8] {
        self.0.y_plane()
    }

    fn u(&self) -> &[u8] {
        self.0.u_plane()
    }

    fn v(&self) -> &[u8] {
        self.0.v_plane()
    }
}

pub struct H264Encoder {
    pub encoder: openh264::encoder::Encoder,
    yuv_buffer: Option<PersistentYuvBuffer>,
}

pub static ENCODE_TO_H264_HISTOGRAM: LazyLock<HistogramVec> = LazyLock::new(|| {
    register_histogram_vec!("encode_to_h264_histogram", "help", &["frame_type"]).unwrap()
});

impl H264Encoder {
    pub fn new(setting: H264EncoderSettings) -> Self {
        let config = openh264::encoder::EncoderConfig::new()
            .intra_frame_period(IntraFramePeriod::from_num_frames(setting.gop))
            .bitrate(BitRate::from_bps(setting.bps));
        let api = OpenH264API::from_source();
        let encoder = openh264::encoder::Encoder::with_api_config(api, config).unwrap();
        Self {
            encoder,
            yuv_buffer: None,
        }
    }
}

impl H264Encoder {
    fn encode_with_encoder(
        encoder: &mut openh264::encoder::Encoder,
        yuv: &PersistentYuvBuffer,
    ) -> Result<Vec<NalInfo>, CaptureError> {
        let encode_to_h264_timer = Instant::now();
        let yuv_source = PersistentYuvView(yuv);
        let encoded_bit_stream = encoder.encode(&yuv_source)?;
        let frame_type_str = match encoded_bit_stream.frame_type() {
            openh264::encoder::FrameType::Invalid => "Invalid",
            openh264::encoder::FrameType::IDR => "IDR",
            openh264::encoder::FrameType::I => "I",
            openh264::encoder::FrameType::P => "P",
            openh264::encoder::FrameType::Skip => "Skip",
            openh264::encoder::FrameType::IPMixed => "IPMixed",
        };
        log::trace!("Encoded to H.264 format");
        let encoded_bit_bytes = bytes::Bytes::from(encoded_bit_stream.to_vec());
        log::trace!("frame_type={:?}, num_layers={:?}", encoded_bit_stream.frame_type(), encoded_bit_stream.num_layers());
        ENCODE_TO_H264_HISTOGRAM
            .with_label_values(&[frame_type_str])
            .observe(duration_to_seconds(
                Instant::now().saturating_duration_since(encode_to_h264_timer),
            ));
        Ok(vec![NalInfo {
            nal_bytes: encoded_bit_bytes,
        }])
    }
}

impl VideoEncoder for H264Encoder {
    fn encode(&mut self, image_info: &dyn ImageInfo) -> Result<Vec<NalInfo>, CaptureError> {
        if self.yuv_buffer.is_none() {
            self.yuv_buffer = Some(PersistentYuvBuffer::new(
                image_info.get_width(),
                image_info.get_height(),
            ));
        }
        self.yuv_buffer.as_mut().unwrap().update(image_info)?;
        // Split borrow: self.encoder (mut) and self.yuv_buffer (shared) are different fields.
        H264Encoder::encode_with_encoder(
            &mut self.encoder,
            self.yuv_buffer.as_ref().unwrap(),
        )
    }

    fn encode_cached(&mut self) -> Result<Vec<NalInfo>, CaptureError> {
        let Some(_) = self.yuv_buffer.as_ref() else {
            return Ok(vec![]);
        };
        H264Encoder::encode_with_encoder(
            &mut self.encoder,
            self.yuv_buffer.as_ref().unwrap(),
        )
    }
}
