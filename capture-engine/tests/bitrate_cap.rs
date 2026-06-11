//! Runtime bitrate-cap smoke tests for the video encoders.
//!
//! Covers the `VideoEncoder::set_bitrate_cap` contract: x264 / VP8 /
//! VP9 / OpenH264 apply and clear a cap without being rebuilt and keep
//! encoding afterwards; AV1 (rav1e) reports the capability as
//! unsupported. Rate behaviour itself (output shrinking under a tight
//! cap) is validated manually end-to-end, not asserted here, to keep
//! these tests deterministic across codecs and platforms.

use desk_capture_engine::model::image_capture::{ImageInfo, ImageType};
use desk_capture_engine::model::video_encoder::VideoEncoder;
use desk_capture_engine::video_encoder::av1_encoder::Av1Encoder;
use desk_capture_engine::video_encoder::h264_encoder::H264Encoder;
use desk_capture_engine::video_encoder::vpx_encoder::VpxEncoder;
use desk_capture_engine::video_encoder::x264_encoder::X264Encoder;
use desk_signal_facade::model::desk_settings::{
    Av1EncoderSettings, H264EncoderSettings, VpxEncoderSettings, X264EncoderSettings,
};
use desk_signal_facade::model::image_capture::{DisplayInfo, DisplayRect};

const WIDTH: u32 = 640;
const HEIGHT: u32 = 480;

struct SyntheticFrame {
    data: Vec<u8>,
}

impl SyntheticFrame {
    /// Deterministic BGRA frame: a gradient background with a
    /// seed-positioned solid block. Compressible like real desktop
    /// content (full-frame noise overflows OpenH264's output buffer,
    /// which is sized from the target bitrate) yet changing between
    /// frames so the encoders have real motion to encode.
    fn new(seed: u32) -> Self {
        let mut data = vec![0u8; (WIDTH * HEIGHT * 4) as usize];
        let block_x = (seed * 37) % (WIDTH - 64);
        let block_y = (seed * 23) % (HEIGHT - 64);
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                let idx = ((y * WIDTH + x) * 4) as usize;
                let in_block = x >= block_x && x < block_x + 64 && y >= block_y && y < block_y + 64;
                if in_block {
                    data[idx] = 255;
                    data[idx + 1] = (seed * 41) as u8;
                    data[idx + 2] = 32;
                } else {
                    data[idx] = (x & 0xFF) as u8;
                    data[idx + 1] = (y & 0xFF) as u8;
                    data[idx + 2] = ((x + y + seed) & 0xFF) as u8;
                }
                data[idx + 3] = 255;
            }
        }
        Self { data }
    }
}

impl ImageInfo for SyntheticFrame {
    fn get_type(&self) -> ImageType {
        ImageType::BGRA
    }
    fn get_data(&self) -> &[u8] {
        &self.data
    }
    fn get_width(&self) -> u32 {
        WIDTH
    }
    fn get_height(&self) -> u32 {
        HEIGHT
    }
}

fn test_display_info() -> DisplayInfo {
    DisplayInfo {
        device_name: r"\\.\TESTDISPLAY".to_string(),
        display_device_name: None,
        desktop_coordinates: DisplayRect {
            left: 0,
            top: 0,
            right: WIDTH as i32,
            bottom: HEIGHT as i32,
        },
        resolutions: vec![],
        attached_to_desktop: true,
        rotation: 0,
    }
}

/// Drives the shared supported-encoder scenario: encode, tighten the
/// cap, keep encoding, clear the cap, keep encoding.
fn assert_cap_cycle(encoder: &mut dyn VideoEncoder, label: &str) {
    let mut produced = 0usize;
    for seed in 1..=5u32 {
        let frame = SyntheticFrame::new(seed);
        produced += encoder
            .encode(&frame, false)
            .unwrap_or_else(|e| panic!("{label}: encode before cap failed: {e:?}"))
            .len();
    }
    assert!(produced > 0, "{label}: no NALs before cap");

    assert!(
        encoder.set_bitrate_cap(Some(500)),
        "{label}: tightening the cap must be supported"
    );
    let mut produced = 0usize;
    for seed in 6..=10u32 {
        let frame = SyntheticFrame::new(seed);
        produced += encoder
            .encode(&frame, false)
            .unwrap_or_else(|e| panic!("{label}: encode under cap failed: {e:?}"))
            .len();
    }
    assert!(produced > 0, "{label}: no NALs under cap");

    assert!(
        encoder.set_bitrate_cap(None),
        "{label}: clearing the cap must be supported"
    );
    let mut produced = 0usize;
    for seed in 11..=15u32 {
        let frame = SyntheticFrame::new(seed);
        produced += encoder
            .encode(&frame, false)
            .unwrap_or_else(|e| panic!("{label}: encode after clear failed: {e:?}"))
            .len();
    }
    assert!(produced > 0, "{label}: no NALs after clearing the cap");
}

#[test]
fn x264_cap_cycle() {
    let mut encoder = X264Encoder::new(X264EncoderSettings::default(), &test_display_info(), 30)
        .expect("x264 build failed");
    assert_cap_cycle(&mut encoder, "x264");
}

#[test]
fn vp8_cap_cycle() {
    let mut encoder = VpxEncoder::new(
        vpx_encode::VideoCodecId::VP8,
        VpxEncoderSettings::default(),
        &test_display_info(),
    )
    .expect("vp8 build failed");
    assert_cap_cycle(&mut encoder, "vp8");
}

#[test]
fn vp9_cap_cycle() {
    let mut encoder = VpxEncoder::new(
        vpx_encode::VideoCodecId::VP9,
        VpxEncoderSettings::default(),
        &test_display_info(),
    )
    .expect("vp9 build failed");
    // VP9's default lag-in-frames buffers the first frames, so skip the
    // strict "output before cap" half and only require that cap calls
    // succeed and encoding keeps running across the whole cycle.
    let mut total = 0usize;
    for seed in 1..=10u32 {
        let frame = SyntheticFrame::new(seed);
        total += encoder
            .encode(&frame, false)
            .expect("vp9 encode failed")
            .len();
    }
    assert!(
        encoder.set_bitrate_cap(Some(500)),
        "vp9 tighten unsupported"
    );
    for seed in 11..=20u32 {
        let frame = SyntheticFrame::new(seed);
        total += encoder
            .encode(&frame, false)
            .expect("vp9 encode under cap failed")
            .len();
    }
    assert!(encoder.set_bitrate_cap(None), "vp9 clear unsupported");
    for seed in 21..=30u32 {
        let frame = SyntheticFrame::new(seed);
        total += encoder
            .encode(&frame, false)
            .expect("vp9 encode after clear failed")
            .len();
    }
    assert!(total > 0, "vp9 produced no output across the cycle");
}

#[test]
fn openh264_cap_cycle() {
    // gop=120 mirrors what `DeskSettings::get_h264_encoder_settings`
    // always passes in production.
    let mut encoder = H264Encoder::new(H264EncoderSettings {
        gop: 120,
        ..Default::default()
    });
    assert_cap_cycle(&mut encoder, "openh264");
}

#[test]
fn av1_reports_unsupported() {
    let mut encoder = Av1Encoder::new(Av1EncoderSettings::default(), &test_display_info())
        .expect("av1 build failed");
    assert!(
        !encoder.set_bitrate_cap(Some(500)),
        "rav1e has no runtime reconfig; set_bitrate_cap must report unsupported"
    );
}
