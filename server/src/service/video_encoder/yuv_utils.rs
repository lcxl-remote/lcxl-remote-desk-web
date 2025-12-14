use std::sync::LazyLock;

use prometheus::{Histogram, register_histogram};
use yuv::{
    YuvChromaSubsampling, YuvConversionMode, YuvPlanarImageMut, YuvRange, YuvStandardMatrix,
    bgra_to_yuv420, rgb_to_yuv420,
};

use crate::{
    desk_error::DeskError,
    model::image_capture::{ImageInfo, ImageType},
};

pub static CONVERT_TO_YUV_HISTOGRAM: LazyLock<Histogram> =
    LazyLock::new(|| register_histogram!("convert_to_yuv_histogram", "help").unwrap());

pub fn convert_image_to_yuv420(
    image_info: &dyn ImageInfo,
) -> Result<YuvPlanarImageMut<'_, u8>, DeskError> {
    let convert_to_yuv_timer = CONVERT_TO_YUV_HISTOGRAM.start_timer();

    let width = image_info.get_width();
    let height = image_info.get_height();

    let src_stride = width * 4;
    let mut planar_image =
        YuvPlanarImageMut::<u8>::alloc(width as u32, height as u32, YuvChromaSubsampling::Yuv420);
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
    Ok(planar_image)
}

pub fn argb_to_i420(width: usize, height: usize, src: &[u8], dest: &mut Vec<u8>) {
    let stride = src.len() / height;

    dest.clear();

    for y in 0..height {
        for x in 0..width {
            let o = y * stride + 4 * x;

            let b = src[o] as i32;
            let g = src[o + 1] as i32;
            let r = src[o + 2] as i32;

            let y = (66 * r + 129 * g + 25 * b + 128) / 256 + 16;
            dest.push(clamp(y));
        }
    }

    for y in (0..height).step_by(2) {
        for x in (0..width).step_by(2) {
            let o = y * stride + 4 * x;

            let b = src[o] as i32;
            let g = src[o + 1] as i32;
            let r = src[o + 2] as i32;

            let u = (-38 * r - 74 * g + 112 * b + 128) / 256 + 128;
            dest.push(clamp(u));
        }
    }

    for y in (0..height).step_by(2) {
        for x in (0..width).step_by(2) {
            let o = y * stride + 4 * x;

            let b = src[o] as i32;
            let g = src[o + 1] as i32;
            let r = src[o + 2] as i32;

            let v = (112 * r - 94 * g - 18 * b + 128) / 256 + 128;
            dest.push(clamp(v));
        }
    }
}

fn clamp(x: i32) -> u8 {
    x.min(255).max(0) as u8
}
