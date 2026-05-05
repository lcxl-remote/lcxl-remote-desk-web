use std::sync::LazyLock;

use prometheus::{Histogram, register_histogram};
use yuv::{
    YuvChromaSubsampling, YuvConversionMode, YuvPlanarImageMut, YuvRange, YuvStandardMatrix,
    bgra_to_yuv420, rgb_to_yuv420,
};

use crate::{
    error::CaptureError,
    model::image_capture::{DirtyRect, ImageInfo, ImageType},
};

pub static CONVERT_TO_YUV_HISTOGRAM: LazyLock<Histogram> =
    LazyLock::new(|| register_histogram!("convert_to_yuv_histogram", "help").unwrap());

pub fn convert_image_to_yuv420(
    image_info: &dyn ImageInfo,
) -> Result<YuvPlanarImageMut<'_, u8>, CaptureError> {
    let convert_to_yuv_timer = CONVERT_TO_YUV_HISTOGRAM.start_timer();

    let width = image_info.get_width();
    let height = image_info.get_height();

    let src_stride = width * 4;
    let mut planar_image =
        YuvPlanarImageMut::<u8>::alloc(width, height, YuvChromaSubsampling::Yuv420);
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

/// Persistent YUV420 buffer that keeps frame state between captures so only changed
/// regions need to be re-converted from BGRA.
///
/// Memory layout: [Y plane | U plane | V plane] contiguous in a single Vec<u8>.
/// Strides match the libvpx / yuv-crate I420 convention: `y_stride = width`,
/// `u/v_stride = ceil(width / 2)` and chroma rows = `ceil(height / 2)`. Using
/// `ceil` (not floor) matters when width or height is odd — the yuv crate's
/// `YuvPlanarImageMut::alloc` allocates chroma with `div_ceil(2)`, so a floor
/// here would mismatch the source slice in `update_full` and panic from
/// `copy_from_slice` (e.g. captured frame 1568x789 → src u_plane=309680
/// vs dest=308896, panic at yuv_utils.rs).
pub struct PersistentYuvBuffer {
    data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    y_offset: usize,
    u_offset: usize,
    v_offset: usize,
    pub y_stride: u32,
    pub u_stride: u32,
    pub v_stride: u32,
}

impl PersistentYuvBuffer {
    pub fn new(width: u32, height: u32) -> Self {
        let y_size = (width as usize) * (height as usize);
        // div_ceil so chroma allocation matches the yuv crate's
        // YuvPlanarImageMut::alloc behaviour for odd dimensions.
        let chroma_w = (width as usize).div_ceil(2);
        let chroma_h = (height as usize).div_ceil(2);
        let uv_size = chroma_w * chroma_h;
        Self {
            data: vec![0u8; y_size + 2 * uv_size],
            width,
            height,
            y_offset: 0,
            u_offset: y_size,
            v_offset: y_size + uv_size,
            y_stride: width,
            u_stride: chroma_w as u32,
            v_stride: chroma_w as u32,
        }
    }

    pub fn y_plane(&self) -> &[u8] {
        &self.data[self.y_offset..self.u_offset]
    }

    pub fn u_plane(&self) -> &[u8] {
        &self.data[self.u_offset..self.v_offset]
    }

    pub fn v_plane(&self) -> &[u8] {
        &self.data[self.v_offset..]
    }

    /// Returns the entire buffer as a contiguous I420 slice (Y | U | V).
    pub fn as_i420_slice(&self) -> &[u8] {
        &self.data
    }

    /// Converts the full frame from BGRA to YUV420 and stores it.
    pub fn update_full(&mut self, image_info: &dyn ImageInfo) -> Result<(), CaptureError> {
        let convert_timer = CONVERT_TO_YUV_HISTOGRAM.start_timer();
        let src_stride = image_info.get_stride();
        let mut temp =
            YuvPlanarImageMut::<u8>::alloc(self.width, self.height, YuvChromaSubsampling::Yuv420);
        match image_info.get_type() {
            ImageType::BGRA => bgra_to_yuv420(
                &mut temp,
                image_info.get_data(),
                src_stride,
                YuvRange::Limited,
                YuvStandardMatrix::Bt601,
                YuvConversionMode::Balanced,
            )?,
            ImageType::RGB => rgb_to_yuv420(
                &mut temp,
                image_info.get_data(),
                src_stride,
                YuvRange::Limited,
                YuvStandardMatrix::Bt601,
                YuvConversionMode::Balanced,
            )?,
        }
        self.data[self.y_offset..self.u_offset].copy_from_slice(temp.y_plane.borrow());
        self.data[self.u_offset..self.v_offset].copy_from_slice(temp.u_plane.borrow());
        self.data[self.v_offset..].copy_from_slice(temp.v_plane.borrow());
        convert_timer.stop_and_record();
        Ok(())
    }

    /// Converts only the dirty regions from BGRA to YUV420 and patches them into the buffer.
    pub fn update_partial(
        &mut self,
        image_info: &dyn ImageInfo,
        rects: &[DirtyRect],
    ) -> Result<(), CaptureError> {
        let src_stride = image_info.get_stride() as usize;
        let src_data = image_info.get_data();
        let y_stride = self.y_stride as usize;
        let u_stride = self.u_stride as usize;
        let v_stride = self.v_stride as usize;

        for rect in rects {
            if rect.width == 0 || rect.height == 0 {
                continue;
            }
            let mut temp = YuvPlanarImageMut::<u8>::alloc(
                rect.width,
                rect.height,
                YuvChromaSubsampling::Yuv420,
            );
            // Offset src to the top-left of the rect; bgra_to_yuv420 uses src_stride for row pitch.
            let src_start = rect.y as usize * src_stride + rect.x as usize * 4;
            match image_info.get_type() {
                ImageType::BGRA => bgra_to_yuv420(
                    &mut temp,
                    &src_data[src_start..],
                    src_stride as u32,
                    YuvRange::Limited,
                    YuvStandardMatrix::Bt601,
                    YuvConversionMode::Balanced,
                )?,
                ImageType::RGB => rgb_to_yuv420(
                    &mut temp,
                    &src_data[src_start..],
                    src_stride as u32,
                    YuvRange::Limited,
                    YuvStandardMatrix::Bt601,
                    YuvConversionMode::Balanced,
                )?,
            }
            let ty = temp.y_plane.borrow();
            let tu = temp.u_plane.borrow();
            let tv = temp.v_plane.borrow();
            let temp_y_stride = temp.y_stride as usize;
            let temp_u_stride = temp.u_stride as usize;
            let temp_v_stride = temp.v_stride as usize;
            let rw = rect.width as usize;
            let rh = rect.height as usize;
            let rx = rect.x as usize;
            let ry = rect.y as usize;

            for row in 0..rh {
                let src_off = row * temp_y_stride;
                let dst_off = self.y_offset + (ry + row) * y_stride + rx;
                self.data[dst_off..dst_off + rw].copy_from_slice(&ty[src_off..src_off + rw]);
            }
            let chroma_w = rw / 2;
            for row in 0..(rh / 2) {
                let src_off = row * temp_u_stride;
                let dst_off = self.u_offset + (ry / 2 + row) * u_stride + rx / 2;
                self.data[dst_off..dst_off + chroma_w]
                    .copy_from_slice(&tu[src_off..src_off + chroma_w]);
            }
            for row in 0..(rh / 2) {
                let src_off = row * temp_v_stride;
                let dst_off = self.v_offset + (ry / 2 + row) * v_stride + rx / 2;
                self.data[dst_off..dst_off + chroma_w]
                    .copy_from_slice(&tv[src_off..src_off + chroma_w]);
            }
        }
        Ok(())
    }

    /// Updates the buffer based on dirty rect information from the image.
    /// - `get_dirty_rects() == None`       → full-frame update
    /// - `get_dirty_rects() == Some([])`   → no change, skip
    /// - `get_dirty_rects() == Some(rects)` → partial update
    pub fn update(&mut self, image_info: &dyn ImageInfo) -> Result<(), CaptureError> {
        match image_info.get_dirty_rects() {
            None => self.update_full(image_info),
            Some(rects) if rects.is_empty() => Ok(()),
            Some(rects) => self.update_partial(image_info, rects),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal `ImageInfo` impl for unit tests — owns a BGRA buffer at the
    /// given dimensions, no dirty-rect info so `update_full` is exercised.
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

    /// `PersistentYuvBuffer::new(width, height)` must allocate chroma planes
    /// large enough to receive a `YuvPlanarImageMut::alloc` of the same
    /// dimensions, regardless of parity. Pre-fix the chroma allocation used
    /// `width / 2` (floor), which mismatched yuv-crate's `div_ceil(2)` and
    /// panicked from `copy_from_slice` whenever capture produced an odd-sided
    /// frame (observed live: 1568x789 from adaptive resolution).
    #[test]
    fn new_allocates_chroma_to_match_yuv_crate_alloc_for_odd_dimensions() {
        let buf = PersistentYuvBuffer::new(1568, 789);
        let temp = YuvPlanarImageMut::<u8>::alloc(1568, 789, YuvChromaSubsampling::Yuv420);
        assert_eq!(
            buf.v_offset - buf.u_offset,
            temp.u_plane.borrow().len(),
            "self.U slice must equal yuv-crate U plane length so update_full can copy_from_slice"
        );
        assert_eq!(
            buf.data.len() - buf.v_offset,
            temp.v_plane.borrow().len(),
            "self.V slice must equal yuv-crate V plane length"
        );
        assert_eq!(buf.u_stride, temp.u_stride);
        assert_eq!(buf.v_stride, temp.v_stride);
    }

    /// Regression: `update_full` panicked at the U-plane copy when the
    /// captured frame had odd width or height. Test exercises both axes
    /// odd (the worst case) by running the actual conversion path.
    #[test]
    fn update_full_does_not_panic_on_odd_dimensions() {
        for (w, h) in [(1568u32, 789u32), (101, 51), (3, 3), (1920, 1081)] {
            let img = StubBgraImage::new(w, h);
            let mut buf = PersistentYuvBuffer::new(w, h);
            buf.update_full(&img)
                .unwrap_or_else(|e| panic!("update_full({w}x{h}) returned error: {e:?}"));
        }
    }

    /// Even-dimension path is the common case and must keep its tight I420
    /// layout (Y=W*H, U=V=(W/2)*(H/2)) — change-detector for any future
    /// refactor that accidentally inflates the buffer.
    #[test]
    fn new_keeps_tight_layout_for_even_dimensions() {
        let buf = PersistentYuvBuffer::new(1920, 1080);
        assert_eq!(buf.y_stride, 1920);
        assert_eq!(buf.u_stride, 960);
        assert_eq!(buf.v_stride, 960);
        assert_eq!(buf.u_offset, 1920 * 1080);
        assert_eq!(buf.v_offset - buf.u_offset, 960 * 540);
        assert_eq!(buf.data.len(), 1920 * 1080 + 2 * 960 * 540);
    }
}
