use std::{
    sync::LazyLock,
    time::{Duration, Instant},
};

use openh264::{
    OpenH264API,
    encoder::{BitRate, IntraFramePeriod},
};
use prometheus::{Histogram, HistogramVec, register_histogram, register_histogram_vec};
use yuv::{
    YuvChromaSubsampling, YuvConversionMode, YuvPlanarImageMut, YuvRange, YuvStandardMatrix,
    bgra_to_yuv420, rgb_to_yuv420,
};

use crate::{
    desk_error::DeskError,
    model::{
        image_capture::{ImageInfo, ImageType},
        settings::H264EncoderSettings,
        video_encoder::{NalInfo, VideoEncoder},
    },
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

pub struct H264Encoder {
    pub encoder: openh264::encoder::Encoder,
}

pub static CONVERT_TO_YUV_HISTOGRAM: LazyLock<Histogram> =
    LazyLock::new(|| register_histogram!("convert_to_yuv_histogram", "help").unwrap());
pub static ENCODE_TO_H264_HISTOGRAM: LazyLock<HistogramVec> = LazyLock::new(|| {
    register_histogram_vec!("encode_to_h264_histogram", "help", &["frame_type"]).unwrap()
});

#[inline]
pub fn duration_to_seconds(d: Duration) -> f64 {
    let nanos = f64::from(d.subsec_nanos()) / 1e9;
    d.as_secs() as f64 + nanos
}

impl H264Encoder {
    pub fn new(setting: H264EncoderSettings) -> Self {
        let config = openh264::encoder::EncoderConfig::new()
            .intra_frame_period(IntraFramePeriod::from_num_frames(30))
            .bitrate(BitRate::from_bps(setting.bps));
        let api = OpenH264API::from_source();
        let encoder = openh264::encoder::Encoder::with_api_config(api, config).unwrap();
        Self { encoder }
    }
}

impl VideoEncoder for H264Encoder {
    fn encode(&mut self, image_info: &dyn ImageInfo) -> Result<NalInfo, DeskError> {
        let convert_to_yuv_timer = CONVERT_TO_YUV_HISTOGRAM.start_timer();

        let width = image_info.get_width();
        let height = image_info.get_height();

        let src_stride = width * 4;
        let mut planar_image = YuvPlanarImageMut::<u8>::alloc(
            width as u32,
            height as u32,
            YuvChromaSubsampling::Yuv420,
        );
        match image_info.get_type() {
            ImageType::BGRA => bgra_to_yuv420(
                &mut planar_image,
                image_info.get_data(),
                src_stride,
                YuvRange::Limited,
                YuvStandardMatrix::Bt601,
                YuvConversionMode::Balanced,
            )?,
            ImageType::RGB => rgb_to_yuv420(
                &mut planar_image,
                image_info.get_data(),
                src_stride,
                YuvRange::Limited,
                YuvStandardMatrix::Bt601,
                YuvConversionMode::Balanced,
            )?,
        };

        log::trace!("Converted to YUV420 format");
        convert_to_yuv_timer.stop_and_record();

        let encode_to_h264_timer = Instant::now();
        let yuv_source = YuvPlanarImageWrapper::<u8>::new(planar_image);

        let encoded_bit_stream = self.encoder.encode(&yuv_source)?;
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
        log::trace!(
            "frame_type={:?}, num_layers={:?}",
            encoded_bit_stream.frame_type(),
            encoded_bit_stream.num_layers()
        );
        ENCODE_TO_H264_HISTOGRAM
            .with_label_values(&[frame_type_str])
            .observe(duration_to_seconds(
                Instant::now().saturating_duration_since(encode_to_h264_timer),
            ));
        Ok(NalInfo {
            nal_bytes: encoded_bit_bytes,
        })
    }
}
