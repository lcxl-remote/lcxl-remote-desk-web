//! Model-path screenshot budget.
//!
//! The screen collector produces a full-resolution PNG (up to a 12 MiB cap),
//! which is fine for the audit/evidence path but far too large and costly to
//! send to a vision model. For the model we re-fit the screenshot: scale it down
//! to a model-friendly dimension and re-encode it as JPEG, stepping the quality
//! down until it fits a byte budget. This runs host-side, after redaction and
//! before the request is built.

use desk_agent_protocol::evidence::EvidenceSnapshot;
use desk_agent_protocol::{AgentOutcome, OperationOutput, ReadContextOutput};
use image::ImageError;
use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;

/// Default longest-edge the model screenshot is scaled to.
pub const DEFAULT_MAX_DIMENSION: u32 = 1280;
/// Default byte budget for the re-encoded model screenshot (~400 KB).
pub const DEFAULT_MAX_BYTES: usize = 400_000;

/// JPEG qualities tried in order until the encoded image fits the budget.
const QUALITY_LADDER: [u8; 5] = [80, 65, 50, 35, 20];
/// Longest-edge floor for the dimension ladder. An image that is already smaller
/// is never upscaled.
const MIN_LONGEST_EDGE: u32 = 320;

#[derive(Debug)]
pub enum ScreenshotFitError {
    Decode(ImageError),
    BudgetExceeded,
}

impl std::fmt::Display for ScreenshotFitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Decode(error) => write!(f, "image decode/encode failed: {error}"),
            Self::BudgetExceeded => f.write_str("image cannot fit the screenshot byte budget"),
        }
    }
}

impl std::error::Error for ScreenshotFitError {}

impl From<ImageError> for ScreenshotFitError {
    fn from(value: ImageError) -> Self {
        Self::Decode(value)
    }
}

/// A screenshot re-fitted for the model: JPEG bytes plus the scaled dimensions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FittedScreenshot {
    pub jpeg: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

impl FittedScreenshot {
    /// The `data:` URL form an OpenAI-compatible vision message expects.
    pub fn to_data_url(&self) -> String {
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&self.jpeg);
        format!("data:image/jpeg;base64,{b64}")
    }
}

/// Fit an encoded image (PNG/JPEG/WebP) into the model budget. It walks a
/// longest-edge dimension ladder and, at each dimension, a JPEG quality ladder.
/// The function succeeds only when the hard byte limit is met; callers never get
/// a best-effort oversized image.
pub fn fit_screenshot_to_budget(
    encoded: &[u8],
    max_dimension: u32,
    max_bytes: usize,
) -> Result<FittedScreenshot, ScreenshotFitError> {
    if max_dimension == 0 || max_bytes == 0 {
        return Err(ScreenshotFitError::BudgetExceeded);
    }
    let image = image::load_from_memory(encoded)?;
    let original_longest = image.width().max(image.height());
    let mut target_longest = original_longest.min(max_dimension);

    loop {
        let scaled = if original_longest > target_longest {
            image.resize(target_longest, target_longest, FilterType::Triangle)
        } else {
            image.clone()
        };
        // JPEG has no alpha channel; flatten to RGB.
        let rgb = scaled.to_rgb8();
        for quality in QUALITY_LADDER {
            let mut buf = Vec::new();
            JpegEncoder::new_with_quality(&mut buf, quality).encode_image(&rgb)?;
            if buf.len() <= max_bytes {
                return Ok(FittedScreenshot {
                    jpeg: buf,
                    width: rgb.width(),
                    height: rgb.height(),
                });
            }
        }

        if target_longest <= MIN_LONGEST_EDGE.min(original_longest) {
            break;
        }
        let next = target_longest.saturating_mul(3) / 4;
        target_longest = next.max(MIN_LONGEST_EDGE.min(original_longest));
    }
    Err(ScreenshotFitError::BudgetExceeded)
}

/// Refit every screenshot entry in an evidence snapshot into a model-ready data
/// URL, in place. This is the **edge-side** step: the raw screen capture is
/// scaled + JPEG-recompressed into a small `data:image/jpeg;base64,...` string
/// stored on the entry's `image_data_url`, so the central orchestrator can attach
/// it as a vision image without ever handling raw bytes (and the multi-MiB
/// original never travels off the machine).
///
/// A screenshot that fails to decode is left without a data URL (the diagnosis
/// proceeds without the image rather than aborting). The raw bytes in the
/// entry's `outcome` are left untouched for the edge's own audit/eval; callers
/// shipping the snapshot off-machine clear them separately.
pub fn refit_snapshot_screenshots(snapshot: &mut EvidenceSnapshot) {
    for entry in &mut snapshot.contexts {
        if entry.image_data_url.is_some() {
            continue;
        }
        if let AgentOutcome::Ok(OperationOutput::ReadContext(
            ReadContextOutput::ScreenCaptureCurrent(shot),
        )) = &entry.outcome
            && let Ok(fitted) =
                fit_screenshot_to_budget(&shot.image, DEFAULT_MAX_DIMENSION, DEFAULT_MAX_BYTES)
        {
            entry.image_data_url = Some(fitted.to_data_url());
        }
    }
}

/// Drop the raw screenshot bytes from every screen entry, in place, keeping the
/// model-ready `image_data_url`. Called before shipping a snapshot off-machine
/// (the remote-collect response): the full-resolution capture (up to 12 MiB) must
/// never travel, only the refit data URL the central brain attaches as a vision
/// image. Run [`refit_snapshot_screenshots`] first so the data URL exists.
pub fn strip_raw_screenshots(snapshot: &mut EvidenceSnapshot) {
    for entry in &mut snapshot.contexts {
        if let AgentOutcome::Ok(OperationOutput::ReadContext(
            ReadContextOutput::ScreenCaptureCurrent(shot),
        )) = &mut entry.outcome
        {
            shot.image = Vec::new();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageFormat, RgbaImage};
    use std::io::Cursor;

    /// Build a noisy RGBA PNG of the given size (noise resists JPEG compression,
    /// so the budget ladder is actually exercised).
    fn noisy_png(width: u32, height: u32) -> Vec<u8> {
        let mut img = RgbaImage::new(width, height);
        let mut seed: u32 = 0x1234_5678;
        for px in img.pixels_mut() {
            // xorshift for deterministic pseudo-random pixels.
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            let b = seed.to_le_bytes();
            *px = image::Rgba([b[0], b[1], b[2], 255]);
        }
        let mut buf = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut Cursor::new(&mut buf), ImageFormat::Png)
            .unwrap();
        buf
    }

    /// A large screenshot is scaled down to the max dimension and re-encoded
    /// within the byte budget.
    #[test]
    fn large_image_is_scaled_and_within_budget() {
        let png = noisy_png(2000, 1500);
        let fitted = fit_screenshot_to_budget(&png, 1280, 400_000).expect("fit");
        assert!(fitted.width <= 1280 && fitted.height <= 1280);
        // Aspect ratio preserved (4:3 → longest edge is width).
        assert_eq!(fitted.width, 1280);
        assert!(
            fitted.jpeg.len() <= 400_000,
            "jpeg {} bytes",
            fitted.jpeg.len()
        );
        // It is a valid JPEG.
        assert_eq!(&fitted.jpeg[0..2], &[0xFF, 0xD8]);
    }

    /// A small image is not upscaled; its dimensions are preserved.
    #[test]
    fn small_image_keeps_dimensions() {
        let png = noisy_png(320, 240);
        let fitted = fit_screenshot_to_budget(&png, 1280, 400_000).expect("fit");
        assert_eq!((fitted.width, fitted.height), (320, 240));
    }

    /// A tiny byte budget fails instead of returning an oversized best-effort JPEG.
    #[test]
    fn impossible_budget_fails_closed() {
        let png = noisy_png(1000, 1000);
        assert!(matches!(
            fit_screenshot_to_budget(&png, 512, 1),
            Err(ScreenshotFitError::BudgetExceeded)
        ));
    }

    /// Invalid image bytes surface a decode error (the caller drops the
    /// screenshot rather than aborting the diagnosis).
    #[test]
    fn invalid_bytes_error() {
        assert!(fit_screenshot_to_budget(b"not an image", 1280, 400_000).is_err());
    }

    /// The data URL carries a JPEG mime and base64 body.
    #[test]
    fn data_url_is_jpeg_base64() {
        let png = noisy_png(64, 64);
        let fitted = fit_screenshot_to_budget(&png, 1280, 400_000).expect("fit");
        let url = fitted.to_data_url();
        assert!(url.starts_with("data:image/jpeg;base64,"));
        assert!(url.len() > "data:image/jpeg;base64,".len());
    }

    /// Refitting a snapshot turns the raw screen capture into a model-ready data
    /// URL on the entry, leaving non-screen entries untouched.
    #[test]
    fn refit_populates_image_data_url() {
        use desk_agent_protocol::{Capability, ImageFormat as ProtoFmt, ScreenCaptureOutput};
        let png = noisy_png(64, 64);
        let shot = AgentOutcome::Ok(OperationOutput::ReadContext(
            ReadContextOutput::ScreenCaptureCurrent(ScreenCaptureOutput {
                display: r"\\.\DISPLAY1".into(),
                format: ProtoFmt::Png,
                width: 64,
                height: 64,
                dpi_x: 96,
                dpi_y: 96,
                window: None,
                image: png,
                truncated: false,
            }),
        ));
        let mut snap = EvidenceSnapshot::record(
            "live",
            "q",
            "2026-06-16T00:00:00Z",
            vec![(Capability::ScreenCaptureCurrent, shot)],
        );
        assert!(snap.contexts[0].image_data_url.is_none());
        refit_snapshot_screenshots(&mut snap);
        let url = snap.contexts[0]
            .image_data_url
            .as_ref()
            .expect("refit produced a data URL");
        assert!(url.starts_with("data:image/jpeg;base64,"));
    }

    /// Stripping raw bytes empties the screen capture while preserving the refit
    /// data URL, so the off-machine snapshot carries only the small image.
    #[test]
    fn strip_clears_raw_bytes_keeps_data_url() {
        use desk_agent_protocol::{Capability, ImageFormat as ProtoFmt, ScreenCaptureOutput};
        let png = noisy_png(64, 64);
        let shot = AgentOutcome::Ok(OperationOutput::ReadContext(
            ReadContextOutput::ScreenCaptureCurrent(ScreenCaptureOutput {
                display: r"\\.\DISPLAY1".into(),
                format: ProtoFmt::Png,
                width: 64,
                height: 64,
                dpi_x: 96,
                dpi_y: 96,
                window: None,
                image: png,
                truncated: false,
            }),
        ));
        let mut snap = EvidenceSnapshot::record(
            "live",
            "q",
            "2026-06-16T00:00:00Z",
            vec![(Capability::ScreenCaptureCurrent, shot)],
        );
        refit_snapshot_screenshots(&mut snap);
        strip_raw_screenshots(&mut snap);
        assert!(snap.contexts[0].image_data_url.is_some());
        if let AgentOutcome::Ok(OperationOutput::ReadContext(
            ReadContextOutput::ScreenCaptureCurrent(shot),
        )) = &snap.contexts[0].outcome
        {
            assert!(shot.image.is_empty(), "raw bytes must be stripped");
        } else {
            panic!("expected a screen capture outcome");
        }
    }

    /// A screenshot entry that fails to decode is left without a data URL rather
    /// than aborting.
    #[test]
    fn refit_skips_undecodable_screenshot() {
        use desk_agent_protocol::{Capability, ImageFormat as ProtoFmt, ScreenCaptureOutput};
        let shot = AgentOutcome::Ok(OperationOutput::ReadContext(
            ReadContextOutput::ScreenCaptureCurrent(ScreenCaptureOutput {
                display: r"\\.\DISPLAY1".into(),
                format: ProtoFmt::Png,
                width: 1,
                height: 1,
                dpi_x: 96,
                dpi_y: 96,
                window: None,
                image: b"not an image".to_vec(),
                truncated: false,
            }),
        ));
        let mut snap = EvidenceSnapshot::record(
            "live",
            "q",
            "2026-06-16T00:00:00Z",
            vec![(Capability::ScreenCaptureCurrent, shot)],
        );
        refit_snapshot_screenshots(&mut snap);
        assert!(snap.contexts[0].image_data_url.is_none());
    }
}
