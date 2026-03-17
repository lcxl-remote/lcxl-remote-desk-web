use std::time::Duration;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::model::{audio_capture::SelectedAudioDevice, image_capture::DisplayInfo};

/// X264 encoder settings
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize, ToSchema)]
#[serde(default)]
pub struct X264EncoderSettings {
    /// Quality (CRF), 0-51, default is 22. Lower is better.
    pub quality: u32,
    /// Group of Pictures, default is 0, which means the encoder will decide the value.
    pub gop: u32,
}

impl Default for X264EncoderSettings {
    fn default() -> Self {
        Self {
            quality: 22,
            gop: 0,
        }
    }
}
/// H264 encoder settings
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize, ToSchema)]
#[serde(default)]
pub struct H264EncoderSettings {
    /// Bitrate in bps (bits per second), default is 10,000,000 bps (10 Mbps)
    pub bps: u32,
    /// Group of Pictures, default is 0, which means the encoder will decide the value.
    pub gop: u32,
}

impl Default for H264EncoderSettings {
    fn default() -> Self {
        Self {
            bps: 4_000_000,
            gop: 0,
        }
    }
}

/// VPX encoder settings
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize, ToSchema)]
#[serde(default)]
pub struct VpxEncoderSettings {
    /// Bitrate in bps (bits per second), default is 50,000,000 bps (50 Mbps)
    pub bps: u32,
    /// Quality (CQ Level), 0-63, default is 25. Lower is better.
    pub quality: u32,
}

impl Default for VpxEncoderSettings {
    fn default() -> Self {
        Self {
            bps: 50_000_000,
            quality: 25,
        }
    }
}

/// Opus encoder settings
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize, ToSchema)]
#[serde(default)]
pub struct OpusEncoderSettings {
    pub sample_rate: u32,
    pub channels: u32,
    pub application: String, // e.g., "audio", "voip", etc.
}

impl Default for OpusEncoderSettings {
    fn default() -> Self {
        Self {
            sample_rate: 48000,
            channels: 2,
            application: "Audio".to_string(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, ToSchema)]
#[serde(default)]
pub struct HotkeySettings {
    /// HOT_KEY_MODIFIERS
    pub fsmodifiers: u32,
    pub vk: u32,
}

impl Default for HotkeySettings {
    fn default() -> Self {
        Self {
            // ALT + CTRL
            fsmodifiers: 3,
            vk: 'L' as u32,
        }
    }
}

/// Private screen settings
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, ToSchema)]
#[serde(default)]
pub struct PrivateScreenSettings {
    /// Optional image path for the private screen background
    pub image_path: Option<String>,
    /// Optional window style for the private screen window
    pub window_style: Option<u32>,
    /// Optional window extended style for the private screen window
    pub window_ex_style: Option<u32>,
    /// Optional hotkey settings for toggling the private screen
    pub hotkey: Option<HotkeySettings>,
    /// Whether private screen is enabled by default
    pub enabled: bool,
}

impl Default for PrivateScreenSettings {
    fn default() -> Self {
        Self {
            image_path: None,
            window_style: None,
            window_ex_style: None,
            hotkey: None,
            enabled: false,
        }
    }
}

/// Desk settings
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, ToSchema)]
#[serde(default)]
pub struct DeskSettings {
    /// Enable D3D debug mode
    pub enable_d3d_debug: bool,
    /// Video device index
    pub video_device_index: u32,
    /// Video encode quality, 0-63, lower is better. Default is 22.
    pub video_quality: u32,
    /// Enable adaptive web page resolution
    pub adaptive_web_page_resolution: bool,
    /// Video zoom ratio (e.g., 50 for 50% zoom)
    pub video_zoom_ratio: u32,
    /// Video frame rate (e.g., 30 fps)
    pub video_fps: u32,
    /// Enable mouse display on the screen
    pub show_mouse: bool,
    /// Selected image capture device
    pub image_capture: Option<String>,
    /// Selected audio capture device
    pub audio_capture: Option<String>,
    /// Video encoder name, None for auto detection
    pub video_encoder: Option<String>,
    /// Selected audio device
    pub audio_device: Option<SelectedAudioDevice>,
    /// Audio encoder name, None for auto detection
    pub audio_encoder: Option<String>,
    /// x264 encoder settings
    pub x264_encoder: Option<X264EncoderSettings>,
    /// h264 encoder settings
    pub h264_encoder: Option<H264EncoderSettings>,
    /// VP8 encoder settings
    pub vp8_encoder: Option<VpxEncoderSettings>,
    /// VP9 encoder settings
    pub vp9_encoder: Option<VpxEncoderSettings>,
    /// opus encoder settings
    pub opus_encoder: Option<OpusEncoderSettings>,

    /// Private screen settings
    pub private_screen: PrivateScreenSettings,
    /// Display name
    pub display_name: Option<String>,
    /// Wayland control mode: portal/uinput/none
    pub wayland_control_mode: Option<String>,
}

impl DeskSettings {
    pub fn get_duration_by_video_fps(&self) -> Duration {
        let mut video_fps = self.video_fps;
        if video_fps <= 0 {
            video_fps = 1;
        }
        Duration::from_micros((1000_000 as f32 / video_fps as f32) as u64)
    }

    pub fn get_x264_encoder_settings(&self) -> X264EncoderSettings {
        if let Some(ref x264_encoder) = self.x264_encoder {
            return x264_encoder.clone();
        }
        // use video_quality to create a default x264 encoder settings
        let mut encoder_settings = X264EncoderSettings::default();
        encoder_settings.quality = self.video_quality;
        encoder_settings
    }

    pub fn get_h264_encoder_settings(&self, display_info: &DisplayInfo) -> H264EncoderSettings {
        if let Some(ref h264_encoder) = self.h264_encoder {
            return h264_encoder.clone();
        }
        // Use video_quality to create a default h264 encoder settings
        let mut encoder_settings = H264EncoderSettings::default();

        // Calculate bps based on resolution and quality
        // video_quality: 0 (highest) to 63 (lowest)
        // BPP (Bits Per Pixel) range: 0.02 (lowest) to 0.20 (highest)
        // For 1080p, 60fps:
        // Quality 0: 1920 * 1080 * 60 * 0.20 = 24.8 Mbps
        // Quality 63: 1920 * 1080 * 60 * 0.02 = 2.48 Mbps

        let width = display_info.desktop_coordinates.width() as u64;
        let height = display_info.desktop_coordinates.height() as u64;
        let fps = if self.video_fps == 0 { 60 } else { self.video_fps } as u64;
        let pixels_per_second = width * height * fps;

        // Linear interpolation for BPP
        // bpp = max_bpp - (video_quality / 63.0) * (max_bpp - min_bpp)
        let max_bpp = 0.20f64;
        let min_bpp = 0.02f64;
        let quality_ratio = (self.video_quality as f64 / 63.0).clamp(0.0, 1.0);
        let bpp = max_bpp - quality_ratio * (max_bpp - min_bpp);

        let mut bps = (pixels_per_second as f64 * bpp) as u32;

        // Cap bps at 100 Mbps to prevent OpenH264 error and excessive bandwidth usage
        let max_bps = 100_000_000;
        if bps > max_bps {
            bps = max_bps;
        }

        encoder_settings.bps = bps;
        encoder_settings
    }

    pub fn get_vp8_encoder_settings(&self) -> VpxEncoderSettings {
        if let Some(ref vp8_encoder) = self.vp8_encoder {
            return vp8_encoder.clone();
        }
        // use video_quality to create a default vp8 encoder settings
        let mut encoder_settings = VpxEncoderSettings::default();
        encoder_settings.quality = self.video_quality;
        encoder_settings
    }

    pub fn get_vp9_encoder_settings(&self) -> VpxEncoderSettings {
        if let Some(ref vp9_encoder) = self.vp9_encoder {
            return vp9_encoder.clone();
        }
        // use video_quality to create a default vp9 encoder settings
        let mut encoder_settings = VpxEncoderSettings::default();
        encoder_settings.quality = self.video_quality;
        encoder_settings
    }
}

impl Default for DeskSettings {
    fn default() -> Self {
        Self {
            enable_d3d_debug: false,
            video_device_index: 0,
            video_quality: 22,
            adaptive_web_page_resolution: false,
            video_zoom_ratio: 100,
            video_fps: 60,
            show_mouse: true,
            image_capture: None,
            audio_capture: None,
            video_encoder: None,
            audio_device: None,
            audio_encoder: None,
            x264_encoder: None,
            h264_encoder: None,
            vp8_encoder: None,
            vp9_encoder: None,
            opus_encoder: None,
            private_screen: PrivateScreenSettings::default(),
            display_name: None,
            wayland_control_mode: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::model::image_capture::DisplayRect;

    use super::*;

    #[test]
    fn test_get_h264_encoder_settings() {
        let mut settings = DeskSettings::default();
        let mut display_info = DisplayInfo::default();
        display_info.desktop_coordinates = DisplayRect {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        settings.video_fps = 60;

        // Test quality 0 (highest)
        settings.video_quality = 0;
        let h264_0 = settings.get_h264_encoder_settings(&display_info);
        // 1920 * 1080 * 60 * 0.20 = 24,883,200
        assert!(h264_0.bps > 24_000_000 && h264_0.bps < 25_000_000);

        // Test quality 63 (lowest)
        settings.video_quality = 63;
        let h264_63 = settings.get_h264_encoder_settings(&display_info);
        // 1920 * 1080 * 60 * 0.02 = 2,488,320
        assert!(h264_63.bps > 2_400_000 && h264_63.bps < 2_500_000);

        // Test with 4K resolution
        display_info.desktop_coordinates = DisplayRect {
            left: 0,
            top: 0,
            right: 3840,
            bottom: 2160,
        };
        settings.video_quality = 0;
        let h264_4k = settings.get_h264_encoder_settings(&display_info);
        // 3840 * 2160 * 60 * 0.20 = 99,532,800
        assert_eq!(h264_4k.bps, 99_532_800);

        // Test with custom high resolution (should hit cap)
        display_info.desktop_coordinates.right = 4000;
        let h264_hit_cap = settings.get_h264_encoder_settings(&display_info);
        assert_eq!(h264_hit_cap.bps, 100_000_000);
    }
}
